# S1 live runbook

Status: **execution preparation for #113**. Every step in this document
that contacts GitHub, spawns a Worker, or mutates the store is
owner-executed and begins only after the owner records authorization on
#113 (see [live-authorization.md](live-authorization.md)). This
document authorizes nothing; a prepared runbook is not permission.

The runbook covers the three S1 variants — `success`, `Attention`, and
`recovery` — against the isolated repository
`shimpeiws/coding-worker-smoke`, using the materials prepared under #73
(`fixtures/s1/` + the prose documents in this directory).

## 1. Readiness audit of the #73 materials

### 1.1 What #73 delivered

| Material | State |
| --- | --- |
| `fixtures/s1/installation-spec.json` | Present; `status: prepared`, `live_execution_authorized: false` |
| `fixtures/s1/seeded-issue.json` | Present; deterministic title/body template, empty label set, per-variant expectations |
| `fixtures/s1/evidence-checklist.json` | Present; S-1–S-8, A-1–A-4, R-1–R-9 rows with variant assignment and required evidence |
| `docs/s1/installation-spec.md` | Present; names the target profile and its unavailable pieces honestly |
| `docs/s1/seeded-issue.md` | Present; fixture procedure including the label rule and reseed rule |
| `docs/s1/evidence-checklist.md` | Present; zero-finding rule and fixture/live separation |
| `scripts/s1/check-prerequisites.py` | Present; read-only preflight, fails closed |

`python3 scripts/s1/check-prerequisites.py` currently reports
`status: prepared`, `installation_ready: false`, zero errors, fixture
content digest
`2b6a6aa3a7a3639c67a646fae17e70d0741e2fb62829fc11ca69843d4e31a05f`.
`prepared` is not readiness: five items remain unresolved across
§1.3–§1.4 — three late-bound identities and two absent target-profile
components.

### 1.2 What is executable today

The only real-host adapter shipped in this tree is `s1-bridge`
(`crates/workloop-cli/src/bin/s1_bridge.rs`) — a scenario-local bridge
that performs **real** `git`/`gh` effects against the isolated
repository and ingests real read-backs through the same
`handle_observation` pipeline a production adapter uses. (The in-process
`fixture` adapter is test infrastructure, not a host adapter.) It is
not the production GitHub adapter. The only Workers exercised so far
are the deterministic shims `scripts/s1/worker-shim.sh` (converging) and
`scripts/s1/worker-shim-nc.sh` (non-converging, for Attention). These
are the same components the retained U8.2/U8.3/U8.4 rehearsals used.

### 1.3 Unresolved identities (resolved during the live preflight, recorded in evidence)

| Identity | Resolution step | Recorded where |
| --- | --- | --- |
| `repository.stable_id` | Read-only host repository read during installation preflight | execution evidence record |
| `reviewer_identity.app_id` | Owner creates the dedicated S1 reviewer App and records its id | owner record + evidence |
| `configuration_identity.store_id` | Fresh store initialized before the run; `store_id` read back | execution evidence record |

Under Profile R (§2.1) the reviewer identity is `github-actions[bot]`
produced by the scenario repository's own Actions workflows — a real
GitHub-authenticated review, **not** the dedicated App. That deviation
must be recorded per run, exactly as U8.2/U8.3 did.

### 1.4 Target-profile blockers (Profile T — not executable today)

| Blocker | Tracking |
| --- | --- |
| Production `github` adapter not shipped (`adapter_available: false`) | U3.x open |
| Dedicated S1 reviewer App does not exist (`reviewer_app_available: false`, `app_id` unresolved) | #149 Stage A / #183 Stage B–C |
| Dedicated invariant-check Invocation role absent — invariants are assessed as Assessment facts, not a separate Invocation | #149 |
| Reference coding-agent Worker runtime never run live (only deterministic shims) | U4.2 evidence is contract-level |
| `S1_PUBLICATION_CREDENTIAL` fine-grained PAT / scoped credential reference not provisioned | owner action |
| Production recovery-intent writer absent — persisted `RetryIntent` rows have no live producer | #96 (U5.6) open |

None of these blocks Profile R. They **do** mean a Profile R run cannot
satisfy the install spec's target profile; the evidence record must say
so (§8, classification).

### 1.5 Retained rehearsals vs this run

`docs/evidence/u8.2.md`, `u8.3.md`, `u8.4.md` are **scenario-bridge
rehearsals**: adapter `s1-bridge`, reviewer `github-actions[bot]`,
credential reference `default`, `host_write: true`, deterministic
Worker shims. They prove the loop mechanics end-to-end but are **not**
evidence for the target `github` + dedicated-reviewer-App +
reference-Worker profile, and they predate the #73 fixture pin
(`materials_revision s1-fixtures-v1`). A #113 run is a **new** run under
the prepared materials with its own run tags, stores, seeds, and
recorded identities; prior rehearsals are cited as methodology, never
as this run's evidence.

### 1.6 Owner decision (communicated 2026-11-15; formal authorization to be recorded on #113)

The owner chose the hybrid path:

- **Execute Profile R now** — all three variants through `s1-bridge` +
  the scenario Worker shims against `shimpeiws/coding-worker-smoke`,
  recording every deviation from the target profile in each evidence
  record. This is the only profile that can execute on the current
  tree.
- **#113 close interpretation** — the Profile R evidence is the
  per-variant live-evidence record for #113's "live evidence exists per
  variant" criterion (live run, exact revisions/config/identities
  recorded, deviations marked). Target-profile certification is
  explicitly **out of scope** for this run and remains tracked in
  #149/#183/#96; Profile R evidence never substitutes for it.
- **Profile T remains available later** — if the §1.4 blockers land,
  §9's delta applies and a fresh target-profile run under new run tags
  would be required; it is not implied by this decision.

## 2. Execution profiles

### 2.1 Profile R — scenario bridge (executable today)

| Element | Value |
| --- | --- |
| Adapter | `s1-bridge` (`target/debug/s1-bridge`) |
| Host effects | real `git push --force-with-lease` + `gh pr create` as the operator's `gh` account |
| Worker | `scripts/s1/worker-shim.sh` (success, recovery) or `scripts/s1/worker-shim-nc.sh` (Attention) |
| Reviewer | `github-actions[bot]` via the scenario repo's Actions `review` workflow |
| Required check | `verify` (scenario repo Actions) |
| Store | fresh SQLite store per variant under a run state directory |

Two processes share one store: `taisan run` drives dispatch admission,
Worker supervision, recovery scheduling, and attention derivation;
`s1-bridge tick` performs source reads, drains pending
`HostActionIntent`s (push/PR), and ingests remote facts (branch state,
change head, check runs, reviews, invariant assessment). In external
adapter mode `taisan run` skips the in-process fixture's source-refresh
and host-outbox phases (`external_adapter` in the tick report) — the
bridge owns those legs.

### 2.2 Profile T — target profile (blocked)

When the §1.4 blockers land, the deltas are: `adapter: "github"`,
`host_actors.publication` bound to the scoped credential reference
(`S1_PUBLICATION_CREDENTIAL`), `worker_argv` pointing at the reference
Worker adapter runtime, reviewer identity `s1-reviewer[bot]` from the
dedicated App, a separate invariant-check Invocation, and a fresh store.
Everything else in this runbook (seed procedure, submit shape,
monitoring queries, evidence template, zero-finding rule) carries over.

## 3. Environment setup (per variant — fresh state each time)

Each variant gets its **own** state directory, store, and
`installation.json`. Never reuse a store across variants: identities
(`w-1`, `e-1`, `cmd-1`…) restart per store and mixing them makes
correlation impossible.

```bash
REPO=/path/to/taisan-checkout          # the tree under test
RUN=<variant>-<UTC>-<suffix>           # e.g. success-20261112T090000Z-a1
STATE=$HOME/s1-$RUN                    # e.g. ~/s1-success-20261112T090000Z-a1
EVID=$STATE/evidence                   # raw envelopes + notes live here
mkdir -p "$STATE" "$EVID"
```

### 3.1 Build the binaries under test

```bash
cd "$REPO"
cargo build -p workloop-cli --bins
TAISAN=$REPO/target/debug/taisan
BRIDGE=$REPO/target/debug/s1-bridge
```

Record the checked-out commit SHA (`git rev-parse HEAD`) — it is the
tested revision for every evidence record. The fixture pin
`f24fce6aa93bc416d84e598a2b18b3055ba0e325` names the *materials* base;
the tested revision is the commit that contains the binaries actually
run (the merge commit of the PR carrying any slice changes, or `HEAD`
for an unchanged tree).

### 3.2 Installation config

Write `$STATE/installation.json` **before** the first `taisan` or
`s1-bridge` invocation — both call `load_or_create`, which would
otherwise publish a default fixture-adapter config.

Success / recovery profile (`worker-shim.sh`):

```json
{
  "actor_id": "actor-s1-<RUN>",
  "actor_kind": "human",
  "host_instance": "github.com",
  "repository": "shimpeiws/coding-worker-smoke",
  "adapter": "s1-bridge",
  "contract_version": 1,
  "capabilities": [
    "repository_read", "branch_read", "issue_read",
    "change_request_read", "change_request_create", "change_request_update",
    "comment_write", "checks_read", "review_findings_read",
    "push_ref"
  ],
  "host_actors": { "publication": "<gh-login>" },
  "required_checks": ["verify"],
  "capacity": 1,
  "worker_argv": ["<REPO>/scripts/s1/worker-shim.sh"],
  "workspace_root": "workspaces",
  "allowed_effects": { "workspace_write": true, "host_write": true, "network": false },
  "budget": {
    "max_invocations": 4,
    "max_review_rounds": 3,
    "runtime_limit_seconds": 14400,
    "artifact_recovery_attempts": 3,
    "artifact_recovery_seconds": 1800,
    "uncertain_threshold_seconds": 900
  },
  "policy": {
    "max_fix_invocations": 3,
    "equivalent_failure_limit": 2,
    "transport_max_retries": 3,
    "effect_reconcile_seconds": 600,
    "no_progress_seconds": 1800,
    "automatic_time_limit_seconds": 14400
  }
}
```

The `budget` block mirrors the values pinned in
`fixtures/s1/installation-spec.json` `configuration_identity.budget`
(`max_invocations 4`, `max_review_rounds 3`, `runtime_limit_seconds
14400`); the three fields the spec does not pin
(`artifact_recovery_*`, `uncertain_threshold_seconds`) carry the
approved defaults. Matching them keeps the recorded configuration as
close to the prepared spec as Profile R allows — the remaining
configuration-identity deviation is `adapter: s1-bridge` vs the pinned
`github`.

Attention profile: identical except `worker_argv` is
`["<REPO>/scripts/s1/worker-shim-nc.sh"]` and `policy.max_fix_invocations`
is lowered to a small bound so the Work-wide fix limit is reachable
within a bounded run — `1` reproduces the U8.3 halt shape (two judgment
rounds, then the bound stops the next fix requeue); `2` adds one more
round. The non-converging shim produces a review-visible gap every
round and each judge→release→requeue consumes one fix invocation; the
owner picks the bound and records it in the evidence record.

`<gh-login>` is the account `gh api user --jq .login` reports — the
publication actor. `host_write: true` is required: the bridge performs
the host writes (this is the recorded Profile R deviation; the target
spec has Workers at `repository_host_write: false` and a narrower
credential).

### 3.3 Read-only host preflight (owner, before any write)

```bash
gh auth status
gh api user --jq .login                                  # → <gh-login>
gh api repos/shimpeiws/coding-worker-smoke \
  --jq '{id, node_id, full_name, default_branch}'
```

The numeric `id` is the repository stable-id candidate for
`repository.stable_id`; `node_id` is the GraphQL identity — record both
in the evidence record and let the owner decide which the spec binds.
Confirm `default_branch` is `main` (the bridge's invariant assessment
diffs `origin/main..head`). Do **not** create issues, branches, or PRs
in this step.

## 4. Shared live procedure

All commands run with `--db "$STATE/workloop.db" --config
"$STATE/installation.json"`. Save every stdout envelope:

```bash
TAISAN=("$TAISAN" --db "$STATE/workloop.db" --config "$STATE/installation.json")
"${TAISAN[@]}" <command> | tee "$EVID/<step>.json"; echo "exit=$?"
```

(`$?` after a pipeline reports `tee`'s status — capture the real code
with `set -o pipefail` or run without `tee` and redirect.)

### 4.1 Seed the issue

Per [seeded-issue.md](seeded-issue.md): fresh `<run-tag>` in the title,
**no labels**, body verbatim from `fixtures/s1/seeded-issue.json`
`body_template`.

```bash
gh issue create --repo shimpeiws/coding-worker-smoke \
  --title "S1 seed <RUN>: normalize greeting output" \
  --body "$(python3 -c 'import json;print(json.load(open("fixtures/s1/seeded-issue.json"))["body_template"])')"
```

Record the new issue number `N`. Verify it carries no labels
(`gh issue view N --repo shimpeiws/coding-worker-smoke --json labels`
→ `[]`). If a seed is consumed/polluted, reseed under a new run tag —
never reuse.

### 4.2 Resolve the issue identity

```bash
"$BRIDGE" --db "$STATE/workloop.db" --config "$STATE/installation.json" \
  issue-ref --issue N | tee "$EVID/issue-ref.json"
```

Expected: pretty JSON `{issue, source_revision, content_digest, state:
"open"}`, exit 0. `source_revision` is the host's `updatedAt` token;
`content_digest` is the canonical issue-content digest —
`s1-issue-v1` over `title\nbody` — the same function the selector binds at admission. These are the
exact `--issue-revision`/`--issue-digest` values `submit` requires — the
CLI never fabricates them. A non-zero exit or `state != "open"` stops
the run.

### 4.3 Submit

```bash
"${TAISAN[@]}" --idempotency-key "<RUN>-submit" submit \
  --intent "S1 <RUN>: implement greet(name) per seeded issue N" \
  --issue "shimpeiws/coding-worker-smoke:N" \
  --issue-revision "<source_revision>" \
  --issue-digest "<content_digest>" \
  --constraint "writable paths: sample module and its test file only" \
  --constraint "no force-push" \
  --constraint "no merge or deploy" \
  --verification "python3 -m unittest discover -s tests -v"
```

Expected: `status:"accepted"`, `command.kind:"submit"`,
`command.id:"cmd-…"`, `work_id:"w-…"`, `execution_id:"e-…"`,
`result` = the work-created effect, exit 0. `accepted` means durably
queued — not started, not complete. Record `command.id`, `work_id`,
`execution_id`, and the full `(origin=cli, actor, idempotency_key)`
triple. A `rejected`/`error`/`unknown` envelope is evidence too — save
it and follow §4.5/§8 before retrying; never mint a fresh key to escape
an `unknown`.

### 4.4 Drive the loop

Two processes, one store:

```bash
# Terminal A — the driver (runs until SIGINT; finishes the current phase)
"${TAISAN[@]}" run --interval 5

# Terminal B — the bridge (repeat until the Work reaches its expected state)
while :; do
  "$BRIDGE" --db "$STATE/workloop.db" --config "$STATE/installation.json" \
    tick --ticks 1 --interval-ms 0
  sleep 20
done
```

`taisan run` prints one envelope only when it exits; per-pass progress
is stderr (`taisan: pass N: M phase(s)`). Tick reports persist as
`tick_report` events in the store. `s1-bridge tick` has no stdout
contract — its diagnostics are stderr and its health signal is store
state; an injected fault exits 137 (§7). GitHub Actions `verify`/`review`
runs take minutes; keep ticking until `work-detail` shows the expected
state. For tighter control, drive single passes instead:
`taisan run --once` and `s1-bridge tick --ticks 1`, alternating with
§4.5 queries between steps — preferred for the recovery variant where
each fault must land on a named tick.

### 4.5 Inspect state

```bash
"${TAISAN[@]}" work-list                          # phases, attention_count
"${TAISAN[@]}" work-detail --work w-N              # executions, invocations, rounds, outcome
"${TAISAN[@]}" attention --work w-N                # open Attention items (versioned ids)
"${TAISAN[@]}" capacity                            # slot + workspace/branch reservations
"${TAISAN[@]}" command-result --command-id cmd-N   # or --origin cli --replay-key <key>
```

Expected: `status:"accepted"` envelopes, exit 0; a missing item is
`accepted` with `result:null` (NotFound), never an error. `error` +
exit 4 means the store could not be read — stop and preserve the store.

### 4.6 Evidence capture rules

- Save every envelope (stdout) and every diagnostic transcript
  (stderr where relevant) under `$EVID/` with a step-ordered name.
- Record UTC timestamps at each step boundary.
- `work-detail` is the correlation source: Work/Execution/Invocation
  ids, review rounds and occurrence ids, host `action_id`s, ChangeHead
  revisions, Outcome and evidence refs all come from it — never from
  prose memory.
- Remote references (issue number, PR number, branch, head SHAs, check
  run ids, review ids) are evidence references, never Core identities.
- Never write credential values, tokens, or `gh` auth material into
  evidence; `credential_ref` appears by name only.

## 5. Variant: success

Goal: `e-…` reaches `succeeded` through the full path — submit →
dispatch → artifact → publish H1 → real checks + review with ≥1 finding
→ judge → fix → publish H2 → reassessment → succeeded. No merge, no
deploy, no zero-finding pass.

| # | Step | Command / action | Expected | Evidence |
| --- | --- | --- | --- | --- |
| S-0 | Preflight | §3.3 reads only | gh identity = `host_actors.publication` | `evidence/preflight.json` notes |
| S-1 | Seed | §4.1–4.2 | issue open, no labels; `issue-ref` state `open` | seed issue #, revision, digest |
| S-2 | Submit | §4.3 | `accepted`, `work_created`, `cmd`/`w`/`e` ids | submit envelope |
| S-3 | Drive | §4.4 loop | dispatch admission → worker Coding invocation → artifact → verification pass → Assessment facts positive → push_ref `Applied` → open_change_request `Applied` (PR created, never merged) | tick reports; bridge stderr; remote branch/PR reads |
| S-4 | H1 review | keep ticking | `verify` + `review` check-runs success on H1; sealed review round on H1 with **≥1 occurrence** | `work-detail` rounds; `gh pr view` checks/reviews |
| S-5 | Judge + release | read occurrence ids from `work-detail`, then `judge-findings --work w-N --expected-version V --head <H1> --decision <occ>:fix …` (fresh `--idempotency-key`), then `release-judgment` same target/head | both `accepted`; Execution requeues `next_role=fix`; fix invocation budget charged once | both envelopes; work-detail before/after |
| S-6 | H2 | keep ticking | fix artifact → verification pass on H2 → push_ref `Applied` (H1→H2) → head recheck → fresh check-runs + review on H2 | remote refs; receipts on original `action_id`s |
| S-7 | Terminal | query until `work-detail` shows outcome | `succeeded` Outcome bound to H2 with frozen evidence refs | final `work-detail`, `work-list` |
| S-8 | Zero-finding check | inspect H1 round occurrences | ≥1 finding on H1 **and** the fix round exercised | if 0 findings → mark `review/fix-unexercised`, reseed under new run tag (§8) |

If the H1 review returns zero findings the run **cannot** satisfy S-5–S-8:
record `review/fix-unexercised`, keep all evidence, and reseed — never
reinterpret a clean review as success.

## 6. Variant: Attention

Goal: bounded non-convergence surfaces actionable Attention and the
Work ends `paused` — never `succeeded`, never silently retried past the
bound, acknowledgement presentation-only.

Use the Attention `installation.json` (§3.2: `worker-shim-nc.sh`,
`policy.max_fix_invocations` set to the owner's chosen small bound).

| # | Step | Command / action | Expected | Evidence |
| --- | --- | --- | --- | --- |
| A-0 | Preflight + fresh state | §3 | separate store/config from success | state dir listing |
| A-1 | Seed + submit + drive | §4.1–4.4 | same accepted path; publish H1; review findings pause `e-…` (`paused_judgment` — "review findings require human disposition") | attention item id, pause reason |
| A-2 | Judge + release rounds | repeat: `judge-findings … --head <Hn> --decision <occ>:fix` (fresh key) then `release-judgment` same target/head; keep ticking between rounds | each pair `accepted`; Execution requeues `next_role=fix`; each fix invocation charges `fix_invocations_used` once | envelopes per round; `fix_invocations_used` progression |
| A-3 | Bound halt | keep ticking/querying after the last release | once `fix_invocations_used` reaches the bound, the next fix requeue halts: `paused` with Attention "fix invocation bound reached; human action required"; `recovery_decisions` classifies `fix_budget_exhausted` | attention id (versioned), work-detail |
| A-4 | Acknowledge | `acknowledge --work w-N --expected-version V --attention <att-id>` (fresh key) | `accepted`; attention listing empties; **Work stays `paused`, outcome still null** — ack changed presentation only | ack envelope; attention before/after; work-detail |
| A-5 | Terminal check | final queries | `paused`, `outcome:null`, no terminal code; **no** cancel/retry/resume issued to close it | final work-detail/list |

The paused state **is** the evidence — leave the Work paused.

## 7. Variant: recovery

Goal: abnormal inputs are reconciled under original identities — no
fresh-key duplicates, no second writer, `unknown` preserved until
read-back, typed terminal outcomes. Uses the success `installation.json`
and the same seed procedure. Prefer manual `--once`/`tick` driving so
each fault lands on a named step.

| # | Fault / drill | Procedure | Expected | Evidence |
| --- | --- | --- | --- | --- |
| R-a | Lost publication response + restart (R-4, R-5) | Submit and drive until the bridge is about to apply `push_ref`. Run one tick armed: `S1_BRIDGE_SCENARIO_FAULTS=shimpeiws/coding-worker-smoke S1_BRIDGE_KILL_AFTER_APPLY=push_ref "$BRIDGE" … tick --ticks 1` → bridge exits 137 **after** the push lands, before the receipt. Then SIGINT/restart `taisan run` and run unarmed ticks. | Push landed remotely; intent unsettled (unknown/pending on original `action_id`); restarted bridge reconciles by host read-back (`verify:equal`-class observations); re-push is a verified no-op; **no second push, no second PR**; run proceeds to a typed outcome | fault tick transcript; pre/post work-detail; remote head reads; applied receipts on original action ids |
| R-b | Partial effect — push landed, create refused (R-4) | On the tick that would run `open_change_request`, arm `S1_BRIDGE_SCENARIO_FAULTS=shimpeiws/coding-worker-smoke S1_BRIDGE_INJECT_FAIL_OPEN_PR=1`. Then unset and continue ticks. | Create leg records a definite no-effect `Failed` on the original `action_id` (push receipt stays `Applied`); transport-bounded retry re-attempts under the same identity; exactly one PR results | failed + applied receipts; PR count check (`gh pr list`) |
| R-c | Restart with live worker (R-5) | While a Worker invocation is in flight, SIGINT `taisan run`; restart it. | Process/request identity reconciled; no second writer for the same Execution; capacity reservation held until verified stop | pre/post capacity; work-detail invocation states |
| R-d | Duplicate submit, identical (R-1) | Re-run §4.3 with the **same** `--idempotency-key` and identical payload | `accepted` envelope returns the **original** `command_id`/decision; still one Work/one Execution | both envelopes; `work-list` count |
| R-e | Duplicate submit, changed payload (R-2) | Same key, different `--intent` text | `rejected`, `reason.code:"idempotency_conflict"`, exit 1; original decision retained; no new state | rejected envelope; `command-result` on original key |
| R-f | Lost command response (R-3) | If any mutation returns `unknown` (exit 6) or the response is lost, recover via `command-result --origin cli --replay-key <key>` or an identical replay — never a fresh key | recorded decision returned; `unknown` never coerced to failure/success | unknown + recovery envelopes |
| R-g | Stale head judgment (R-6) | After H2 exists, attempt `judge-findings --head <H1>` | `rejected` (stale expected head); H1 review stays evidence for H1 only; projection keeps H2 current | rejected envelope; work-detail |
| R-h | Budget exhaustion → retry (R-8) | Optional bounded sub-scenario: separate state dir with `budget.max_invocations: 1` (or smallest bound), submit, drive to exhaustion | Execution `failed`/`stopping` with the exhausted limit recorded, WIP preserved; `retry --expected-execution e-…` creates E2 claiming the same workspace/branch | envelopes; work-detail E1/E2 |
| R-i | Duplicate observation (R-9) | Inherent — the bridge re-emits the same remote facts every tick | ingest receipts show `Duplicate`; one recorded fact; no second intent/effect | tick reports / observation receipts in store |
| R-j | Adapter outage (R-7) | No safe injection exists on Profile R; record `absent` with reason unless the owner devises one (e.g., temporary `gh` auth removal — owner discretion, must restore) | Observations `unavailable` ≠ no findings; no completion claimed; recovery observations after restore | — |

Fault hooks are fail-closed: they arm only in a debug build, only when
`S1_BRIDGE_SCENARIO_FAULTS` names exactly `shimpeiws/coding-worker-smoke`,
and only for the hook variable shown. Always unset both variables before
continuing the run; an armed hook left set will fault the next matching
leg.

## 8. Stop conditions and evidence rules

Stop the variant and preserve everything when:

- Any envelope shows `unknown` that does not resolve via
  `command-result`/identical replay — do not mint fresh keys.
- A second writer or second PR/branch appears for the same Work —
  that is itself evidence; capture it, then stop.
- The store returns `error`/exit 4 — preserve the store file untouched.
- Merge/deploy would be required to proceed — never; `succeeded` is the
  terminal claim, merge is out of scope by definition.
- A review returns zero findings on the success path — mark
  `review/fix-unexercised` and reseed under a new run tag.

Classification statement every evidence record must carry: results are
**live** (real GitHub/Worker/store) but **Profile R** — adapter
`s1-bridge`, reviewer `github-actions[bot]`, publication actor = the
operator's `gh` login under credential reference `default`,
`host_write: true`, deterministic scenario Worker. They are not
evidence for the target profile in §1.4.

## 9. Target-profile delta (Profile T — reference only)

When §1.4 blockers land: adapter `github`, `S1_PUBLICATION_CREDENTIAL`
scoped PAT (contents/PRs/issues write, checks read) bound as the
publication credential reference, `s1-reviewer[bot]` App reviews on the
isolated repo only, reference Worker runtime via `worker_argv`,
dedicated invariant-check Invocation, `repository_host_write: false`
for Workers, and resolved `repository.stable_id`/`app_id`/`store_id`.
Seed/submit/monitor/evidence sections carry over unchanged; the bridge
is replaced by the production adapter's own drain and the fault hooks
do not exist — recovery faults must be injected at the adapter/host
boundary instead. Re-run all three variants under new run tags; Profile
R evidence never substitutes.
