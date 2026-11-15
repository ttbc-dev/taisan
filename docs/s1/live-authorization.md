# S1 live authorization — draft for owner record

**Status: DRAFT.** This text is a proposal for the owner to review,
edit, and record on issue #113 (or wherever the owner designates)
**before** any live step in [live-runbook.md](live-runbook.md) begins.
The agent preparing #113 does not record, sign, or imply this
authorization. Recording it is the owner's deliberate act; bracketed
fields must be filled or explicitly approved by the owner.

Authorization precedes every live write: no `submit`, no Worker launch,
no `git push`, no `gh pr create`, and no store mutation on the isolated
installation may happen before the recorded authorization exists.

---

## Draft authorization text

> **S1 live scenario authorization — <issue comment permalink or record id>, recorded <UTC timestamp> by <owner name>.**
>
> I, <owner name>, owner of `shimpeiws/taisan`, authorize the live
> execution of the S1 representative acceptance scenario under the
> implementation start gate (`docs/design-closure.md`), scoped as
> follows:
>
> - **Target repository**: `shimpeiws/coding-worker-smoke` only — the
>   isolated scenario repository. No other repository is in scope; the
>   installation allowlist contains exactly this repository.
> - **Profile**: Profile R (scenario bridge) as defined in
>   `docs/s1/live-runbook.md` §2.1 — adapter `s1-bridge`, Worker
>   `scripts/s1/worker-shim.sh` / `worker-shim-nc.sh`, reviewer
>   `github-actions[bot]` via the scenario repository's Actions,
>   publication actor `<gh-login>` under credential reference `default`,
>   `host_write: true`. **Recorded deviation**: this is not the target
>   profile (`github` adapter + dedicated `s1-reviewer` App +
>   `S1_PUBLICATION_CREDENTIAL` + reference Worker); evidence will be
>   classified Profile R and will not be claimed as target-profile
>   acceptance. [If the owner instead intends the target profile,
>   replace this clause — the target profile is not executable on the
>   current tree; see runbook §1.4/§1.6.]
> - **Variants**: `success`, `Attention`, and `recovery` — each under
>   its own run tag, state directory, store, and seeded issue, per
>   `docs/s1/live-runbook.md` §5–§7.
> - **Seeded issues**: created manually per
>   `fixtures/s1/seeded-issue.json` with no labels; each carries a fresh
>   `<run-tag>`; polluted seeds are reseeded, never reused.
> - **Prohibitions**: no merge, no deploy, no unconditional force-push
>   (host writes are limited to the recorded `push_ref` /
>   `open_change_request` / `comment_write` capabilities, applied by the
>   bridge under `--force-with-lease` guards and persisted intents);
>   no labels applied to seeds; no credential value in any fixture,
>   config, log, or evidence export; no fresh-key duplicate Commands to
>   escape `unknown`; Workers hold no host-mutation credentials beyond
>   the recorded Profile R bridge effects.
> - **Evidence retention**: per-variant evidence records drafted from
>   `docs/s1/live-evidence-template.md` into `docs/evidence/` via a
>   follow-up PR, with raw envelopes retained under the run state
>   directories `<state-dir>/evidence/`; the #113 summary comment links
>   every record.
> - **Validity**: this authorization covers runs started before
>   <expiry UTC timestamp> (suggested: <date + 7 days>) and remains
>   valid only while the tested revision, installation configuration
>   identity, and isolated repository identity are unchanged. Any change
>   to adapter, Worker, reviewer, credential reference, capability set,
>   or repository requires a new authorization.
> - **Stop authority**: the owner may halt any variant at any time; a
>   halted run's partial evidence is retained and classified, never
>   discarded or reinterpreted.

---

## Owner fill-in checklist

| Field | Value |
| --- | --- |
| Owner name / recorded-by | |
| Record location (issue comment permalink) | |
| Recorded-at (UTC) | |
| Profile chosen (R / T / other) | |
| Publication actor `<gh-login>` | |
| Variants authorized | success / Attention / recovery (all three recommended) |
| Evidence retention path confirmed | `docs/evidence/` + run state dirs |
| Expiry timestamp | |
| Tested revision window (branch/commit range allowed) | |

## What this draft deliberately does not do

- It does not resolve `repository.stable_id`, `reviewer_identity.app_id`,
  or `store_id` — those are late-bound and recorded in execution
  evidence, not pre-authorized.
- It does not authorize the target profile, the production adapter, the
  reviewer App, or the reference Worker — those remain blocked per
  runbook §1.4 and tracked in #149/#183/#96.
- It does not approve merging any resulting PR or evidence PR; merge
  decisions stay with the owner under normal review.
