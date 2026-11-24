# S1 live acceptance checklist (#113)

Executable checklist for issue #113's acceptance criteria. Each item
names **who** acts, **what** is done, and **where** it is recorded.
Roles: `owner` = the human owner (only the owner may authorize or
execute live steps); `operator` = whoever the owner designates to run
the commands (may be the owner); `agent` = the preparing agent (offline
work only — never live steps, never authorization).

> **Owner-operations procedure — private references.** This checklist
> drives live steps only the owner can execute, against the private
> repository `shimpeiws/coding-worker-smoke`; its links and `#N`
> citations resolve only for the owner. Retained as a record under the
> owner publication decision (#438).

Record locations: `RUNBOOK` = [live-runbook.md](live-runbook.md),
`AUTH` = [live-authorization.md](live-authorization.md), `TEMPLATE` =
[live-evidence-template.md](live-evidence-template.md), `#113` = issue
comments, `docs/evidence/` = committed evidence records, `$STATE` = the
per-variant run state directory (`workloop.db`, `installation.json`,
`evidence/` envelopes).

## AC-1 — explicit owner authorization before live execution

| # | Who | What | Recorded where |
| --- | --- | --- | --- |
| 1.1 | owner | Reviews AUTH draft + RUNBOOK §1 audit; chooses Profile R or defers for Profile T | #113 comment (decision) |
| 1.2 | owner | Records the authorization text (scope, variants, profile, evidence retention, expiry) on #113 **before** any live step | #113 comment permalink → copied into each evidence record's Authorization field |
| 1.3 | operator | Confirms no `submit`/Worker/`git push`/`gh pr create`/store mutation occurred before 1.2's timestamp | runbook gate; noted in evidence record |
| 1.4 | agent | Verifies the authorization comment exists and names the profile/variants/scope actually used | #113 comment |

## AC-2 — live evidence per variant with exact revisions/config and identities

For **each** of `success`, `Attention`, `recovery` (fresh run tag, state
dir, store, seed per variant):

| # | Who | What | Recorded where |
| --- | --- | --- | --- |
| 2.1 | operator | Executes RUNBOOK §3–§4 setup + seed + submit; saves every envelope under `$STATE/evidence/` | `$STATE/evidence/*.json` |
| 2.2 | operator | Drives `taisan run` + `s1-bridge tick` to the variant's expected state (RUNBOOK §5/§6/§7); captures `work-detail`/`attention`/`capacity` at each checkpoint | `$STATE/evidence/` + transcripts |
| 2.3 | operator | Completes TEMPLATE: Work/Execution/Invocation/`action_id` identities, `(origin, actor, idempotency_key)`→`command_id` for every Command, tested revision, sanitized config identity incl. late-bound `store_id`/`stable_id`, remote refs (issue/PR/heads/checks/reviews), result, fixture/live + Profile R classification | `docs/evidence/u8.1-live-<variant>.md` (draft, via PR) |
| 2.4 | operator | Fills the criterion-coverage table: every applicable S/A/R row marked `pass`/`deviation`/`absent`/`invalid` with an evidence pointer — no unmarked cells | same record |
| 2.5 | operator | Runs the zero-finding check: H1 review occurrences ≥1 for success; if 0 → `review/fix-unexercised`, reseed under new run tag, never claim success | TEMPLATE §Zero-finding check |
| 2.6 | owner | Confirms no merge/deploy occurred and PRs remain open; confirms `succeeded` (success) / `paused`+Attention (Attention) / typed outcomes (recovery) match expectations | #113 comment |

## AC-3 — approve and retain the evidence

| # | Who | What | Recorded where |
| --- | --- | --- | --- |
| 3.1 | owner | Reviews each variant's evidence record incl. deviations (Profile R identities) and approves or rejects | TEMPLATE §Owner approval; #113 comment |
| 3.2 | operator/agent | Opens the evidence PR adding `docs/evidence/u8.1-live-*.md`; links it on #113 | PR + #113 comment |
| 3.3 | owner | Posts the #113 summary: evidence record links, per-variant outcomes, remaining gaps (#149/#183/#96 and any `absent`/`invalid` rows) | #113 comment |
| 3.4 | agent | Removes `agent:running` from #113 at terminal state | issue labels |

## Explicit non-goals (checked, not assumed)

| # | Who | What | Recorded where |
| --- | --- | --- | --- |
| N.1 | owner+agent | No merge/auto-merge/deploy of the scenario PRs or any repo PR as part of acceptance | PR states remain open; noted in evidence |
| N.2 | agent | No live step executed or authorized by the agent at any point | this PR's statement + #113 record |
| N.3 | operator | Profile R evidence is never presented as target-profile acceptance; §1.4 blockers stay tracked in #149/#183/#96 | evidence classification section |
