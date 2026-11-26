# S1 Stage C runbook — re-evidence under dedicated identities (#183)

Status: **execution preparation for #183**. Every step in this
document that contacts GitHub, mutates the scenario repository,
spawns a Worker, or mutates a store is owner-executed and begins only
after the owner records authorization on #183. This document
authorizes nothing; a prepared runbook is not permission.

> **Owner-operations procedure — private references.** Every live step
> here runs only for the owner, against the private repository
> `shimpeiws/coding-worker-smoke`; links, `#N` citations, owner-local
> credential paths, and recorded App/installation identities are
> verbatim owner context and do not resolve publicly. Retained as a
> record under the owner publication decision (#438).

Stage C re-runs the S-3a (invariant-check) and S-5 (reviewer
identity) legs of the S1 success path — and, per the owner's Stage C
scope decision, all three variants — after Stage A (#190, merged)
landed the dedicated `InvariantCheck` Invocation role and the
reviewer-actor binding, and Stage B registered the dedicated
`s1-reviewer` GitHub App on the host.

The live procedure itself is unchanged: follow
[live-runbook.md](live-runbook.md) §3–§8 exactly, with the deltas in
§3 below. This document lists only what differs and what must be
true before the first live step.

## 1. What Stage A changed that this run exercises

- `InvocationRole::InvariantCheck` is admitted by the publish gate
  (`waiting → ready`, same Execution reservation) after every
  producing Coding/Fix Invocation, runs the recorded `worker_argv`
  against a per-Invocation snapshot copy of the workspace, and must
  exit 0 (`ProcessState::Exited{exit_code:0}`) before `push_ref` is
  issued. Its durable record — own `invocation_id`, lifecycle,
  receipts — is the S-3a evidence.
- `host_actors.review` binds the review lane: `seal_round` seals a
  review round only for observations whose `source` equals the bound
  actor. A review authored by `github-actions[bot]` is still recorded
  as an observation but can never seal a round, satisfy the review
  lane, or trigger the findings pause.
- `host_credentials` gives each bound role its own credential slot;
  the review role's slot names how the reviewer App authenticates.

## 2. Stage B state (completed)

| Item | Value |
| --- | --- |
| App | `s1-reviewer`, app id `5767391`, owner `shimpeiws` |
| Installation | `182142418`, `repository_selection: selected`, scope `["shimpeiws/coding-worker-smoke"]` only |
| Effective permissions | `contents: read`, `pull_requests: write`, `metadata: read` |
| Bot actor | `s1-reviewer[bot]` (user id `356876559`) |
| Local material | `~/.config/coding-worker/github-apps/s1-reviewer.{json,pem}` (pem mode `0600`) |
| Resolved spec copy | produced during Stage B; regenerate per run state dir |

## 3. Required deltas vs the #113 Profile R runbook

### 3.1 `installation.json` — bind the review actor

Add the review role to `host_actors` and give it its own credential
slot. The success/recovery template becomes:

```json
  "host_actors": {
    "publication": "<gh-login>",
    "review": "s1-reviewer[bot]"
  },
  "host_credentials": {
    "review": "command:<REPO>/scripts/s1/mint-app-token.py"
  },
```

- `<REPO>` is the absolute path of the checkout under test; the
  `command:` provider splits argv on ASCII whitespace with no shell
  and no `~` expansion, so the path must be absolute and contain no
  spaces. `scripts/s1/mint-app-token.py` (added in this change) mints
  a fresh installation token per resolution from the Stage B sidecar
  record — under `s1-bridge` nothing resolves the slot, but the
  recorded `credential_ref` names a working resolver, not a placeholder.
- Without `host_actors.review`, `seal_round` applies no actor check
  and a `github-actions[bot]` review would seal — the S-5 criterion
  fails. With it, only `s1-reviewer[bot]` reviews seal rounds.
- Everything else in the §3.2 template is unchanged
  (`adapter: "s1-bridge"`, `host_write: true`, budget/policy blocks).

### 3.2 Scenario Worker shims — verify-only check snapshots

The invariant check re-runs the recorded `worker_argv` against a
per-Invocation snapshot copy of the workspace, stored under a
`check-<invocation>` directory name. The shims derive their work
branch from the workspace directory name (`ws-<work_id>` →
`work/<work_id>`), so inside a `check-*` snapshot the derived branch
does not exist. Both `scripts/s1/worker-shim.sh` and
`scripts/s1/worker-shim-nc.sh` now detect that case — derived branch
absent while HEAD still names a `work/*` branch — adopt the branch
HEAD already names, and run the verification argv read-only against
the exact state the producing Invocation left behind. Without this,
the check would mint a fresh `work/check-*` branch from `origin/main`
and verify unrelated state; the exit code still passed, but the
check's durable record would not describe the produced head. A fresh
clone sits on the default branch, so the detection cannot misfire on
a real first Invocation. `worker-shim.sh` additionally gained a
verify-only third round for the same-branch post-fix state.

### 3.3 Scenario repository — review posts as the App

The smoke repository's `.github/workflows/s1-review.yml` must mint an
installation token for `s1-reviewer` instead of using
`secrets.GITHUB_TOKEN`. This is an owner-applied change to
`shimpeiws/coding-worker-smoke` (a different repository from this
one), plus two repository secrets. Both are GitHub writes and are
owner-executed:

1. Add repository secrets (values never enter evidence or logs):

   ```bash
   gh secret set S1_REVIEWER_APP_ID \
     --repo shimpeiws/coding-worker-smoke --body 5767391
   gh secret set S1_REVIEWER_APP_PRIVATE_KEY \
     --repo shimpeiws/coding-worker-smoke \
     < ~/.config/coding-worker/github-apps/s1-reviewer.pem
   ```

   (`--body` reads the value literally; for the PEM use stdin
   redirection as shown so the key never appears in argv.)

2. Replace `.github/workflows/s1-review.yml` on `main` with:

   ```yaml
   name: s1-review
   on:
     pull_request:
       types: [opened, synchronize, reopened]
   permissions:
     contents: read
     pull-requests: write
   jobs:
     review:
       name: review
       runs-on: ubuntu-latest
       steps:
         - uses: actions/checkout@v4
           with:
             ref: ${{ github.event.pull_request.head.sha }}
         - uses: actions/setup-python@v5
           with:
             python-version: "3.12"
         - name: mint reviewer App token
           id: app-token
           uses: actions/create-github-app-token@64ab729a6294000f5aa4a4bc0b73d1f32bc45510 # v3.3.0
           with:
             app-id: ${{ secrets.S1_REVIEWER_APP_ID }}
             private-key: ${{ secrets.S1_REVIEWER_APP_PRIVATE_KEY }}
             owner: shimpeiws
             repositories: coding-worker-smoke
         - name: review diff and post findings
           env:
             GH_TOKEN: ${{ steps.app-token.outputs.token }}
             PR_NUMBER: ${{ github.event.pull_request.number }}
             HEAD_SHA: ${{ github.event.pull_request.head.sha }}
           run: bash .github/scripts/s1-review.sh
   ```

   `.github/scripts/s1-review.sh` is unchanged — it already posts
   `POST /repos/{o}/{r}/pulls/{n}/reviews` with `event=COMMENT` and
   `commit_id=$HEAD_SHA`; only the token's identity changes, so the
   review is authored by `s1-reviewer[bot]`.

3. The change must be on the scenario repository's default branch
   **before** the runs: `work/w-*` branches are cut from `origin/main`
   and `pull_request` workflows execute the head's copy, so branches
   created after the merge carry the App-token workflow.

4. Optional owner pre-check (no scenario state touched): open a
   throwaway PR on the smoke repository and confirm the `review`
   check-run succeeds and `gh api repos/shimpeiws/coding-worker-smoke/
   pulls/<n>/reviews --jq '.[].user.login'` reports
   `s1-reviewer[bot]`. Close the PR afterwards.

## 4. Pre-run checklist (owner, before the first live step)

1. Record Stage C authorization on #183 (scope: all three variants,
   Profile R + dedicated reviewer App + invariant-check Invocation;
   same prohibitions as the #113 authorization — no merge, no deploy,
   no unconditional force-push).
2. Apply §3.3 to `coding-worker-smoke` and verify with the optional
   pre-check.
3. Build the binaries under test per live-runbook §3.1 and record the
   tested revision (`git rev-parse HEAD`).
4. Per variant, create a fresh state dir and write
   `installation.json` per §3.1 — never reuse a store.
5. Regenerate the resolved install-spec copy with
   `reviewer_identity.app_id` filled and run
   `python3 scripts/s1/check-prerequisites.py --install-spec <copy>`;
   `repository.stable_id` resolves from the §3.3 read-only preflight
   (`gh api repos/shimpeiws/coding-worker-smoke`) and
   `configuration_identity.store_id` reads back after the fresh store
   opens — both are recorded in the evidence record, not pre-filled.

## 5. Variant notes

The live-runbook variant sections (§5 success, §6 Attention, §7
recovery) apply unchanged except where noted:

- **Success / recovery** — `worker-shim.sh`, budget block as
  templated. Invocation accounting is now exactly
  `coding + invariant-check + fix + invariant-check = 4`, which fits
  `max_invocations: 4` with no headroom: any extra admission exhausts
  the budget. Do not raise the bound — it is pinned in
  `configuration_identity.budget`.
- **Attention** — `worker-shim-nc.sh`, and set
  `policy.max_fix_invocations: 1`. The same arithmetic gives
  `coding + check + fix + check = 4` and the fix bound halts the next
  requeue, preserving the U8.3 halt shape. `max_fix_invocations: 2`
  would need six invocations and hit `max_invocations` first — a
  different (still Attention-classified) halt reason; the owner picks
  and records the bound.
- **Recovery R-h** — the U8.x recipe armed `max_invocations: 1`; with
  the invariant check that bound is now spent before publication can
  even be attempted, so the exhaustion lands at check admission
  instead of at the fix admission the drill intends. Use
  `max_invocations: 2` for the R-h sub-scenario: `coding + check`
  publishes H1, the findings pause/judge/release proceeds, and the
  fix admission then records the exhausted limit — the same
  fail-then-retry shape as before. All other fault hooks are
  unaffected: the check sits strictly before `push_ref`, so R-a/R-b
  still land on the named legs.

## 6. Evidence the consolidated record must show

For S-3a and S-5 to carry **no deviation rows**, the record must
cite:

- **S-3a**: per publish, an `InvariantCheck` Invocation with its own
  `invocation_id`, `Done` state, `ProcessState::Exited{exit_code:0}`,
  and receipts — distinct from the producing Coding/Fix Invocation.
  `work-detail` output is the correlation source.
- **S-5**: sealed review rounds whose `source` is
  `s1-reviewer[bot]` — never `github-actions[bot]` — plus the remote
  read-back (`gh api …/pulls/<n>/reviews` showing
  `user.login == "s1-reviewer[bot]"`) and the App registration
  citation (app id `5767391`, installation `182142418`).
- The remaining Profile R deviations stay recorded for the other
  legs: adapter `s1-bridge`, deterministic Worker shims, publication
  actor = operator `gh` login under credential `default`,
  `host_write: true`, production `github` adapter and reference
  Worker runtime absent. These are unchanged by Stage C and are not
  S-3a/S-5 deviations.

## 7. Open questions

- Whether the evidence record treats the review *round* (sealed
  `ReviewRound.source`) as the "review Invocation record" #183 names,
  given no `InvocationRole::Review` admission path exists on this
  tree — reviews arrive as observations, and the role is vocabulary +
  budget accounting only. The sealed round is the durable review
  record the live path produces; flag for owner confirmation when
  writing the record.
- `actions/create-github-app-token` v3.3.0 is pinned by commit SHA;
  bump deliberately, never to a floating tag.
