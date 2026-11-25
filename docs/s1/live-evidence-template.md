# S1 live evidence record — template

One file per variant per run, drafted into `docs/evidence/` (suggested
name `u8.1-live-<variant>.md`) after the owner-authorized run completes.
Copy this skeleton and fill every field; a field that does not apply is
marked `n/a` with a reason, never left blank. Bracketed `[owner: …]`
fields are entered by the owner at record time.

> **Owner-operations material — private references.** This template
> produces records of owner-executed runs against the private
> repository `shimpeiws/coding-worker-smoke`; citations to it resolve
> only for the owner. Retained under the owner publication decision
> (#438).

Claims boundary statement (required verbatim, adjusted only for facts):
state plainly that this is a **live** result — real store, real Worker
process, real GitHub effects and read-backs — and name the profile
(`s1-bridge` scenario bridge, `github-actions[bot]` reviewer,
deterministic scenario Worker) so the record can never be misread as
target-profile acceptance.

---

# S1 live evidence — <variant> (<run-tag>)

## Run identity

| Field | Value |
| --- | --- |
| Variant | `success` / `Attention` / `recovery` |
| Run tag | `<run-tag>` |
| State directory | `<path>` (contains `workloop.db`, `installation.json`, `evidence/`) |
| Executed at (UTC) | `<start>` – `<end>` |
| Operator | `[owner: name]` |
| Authorization | `[owner: #113 comment permalink + recorded-at UTC]` |
| Profile | Profile R — `s1-bridge` + scenario Worker shim (see claims boundary) |

## Tested revision and materials

| Field | Value |
| --- | --- |
| Tested revision (commit containing the run binaries) | `<git rev-parse HEAD at build>` |
| Materials revision | `s1-fixtures-v1` |
| Fixture content digest | `2b6a6aa3a7a3639c67a646fae17e70d0741e2fb62829fc11ca69843d4e31a05f` |
| Preflight result at run start | `python3 scripts/s1/check-prerequisites.py` output summary |

## Configuration identity (sanitized)

Copy from `installation.json`; credential values never appear —
`credential_ref`/`host_actors` by name only.

| Field | Value |
| --- | --- |
| `actor_id` / `actor_kind` | |
| `host_instance` | `github.com` |
| `repository` | `shimpeiws/coding-worker-smoke` |
| `adapter` | `s1-bridge` |
| `contract_version` | `1` |
| `capabilities` | |
| `host_actors.publication` | `<gh-login>` |
| `required_checks` | `["verify"]` |
| `capacity` | `1` |
| `worker_argv` | `scripts/s1/worker-shim.sh` or `worker-shim-nc.sh` |
| `allowed_effects` | `{workspace_write: true, host_write: true, network: false}` |
| `policy` | (incl. `max_fix_invocations` for Attention) |
| `store_id` (late-bound, resolved at store open) | `<from store/config identity read-back>` |
| `repository.stable_id` (resolved at host preflight) | `<gh repo view id>` |

## Command / scenario identity

| Step | `(origin, actor, idempotency_key)` | `command_id` | Envelope file |
| --- | --- | --- | --- |
| submit | `(cli, <actor>, <key>)` | `cmd-…` | `evidence/NN-submit.json` |
| …each mutation… | | | |

## Recorded identities

| Kind | Value |
| --- | --- |
| Work / Executions | `w-…` / `e-…` (list every execution incl. retries) |
| Invocations | `inv-…` per role (Coding/Fix/…), states |
| Artifacts | artifact path, `produced` digest, verification result |
| Host actions (`action_id`) | each `push_ref` / `open_change_request` / `comment_write` / metadata intent: id, kind, status (`Applied`/`Failed`/`Unknown`→resolved), receipts |
| Review rounds | round id, head bound, occurrences (id, severity, position) |
| Attention items | versioned `attention_id`s, ack command ids |
| Recovery intents | `rintent-…` rows if any (note: no production writer on this revision — #96) |
| Terminal outcome | final Execution state + Outcome (`succeeded` / `paused` / `failed` / `cancelled`), `outcome:null` where applicable, terminal code/detail |

## Seeded issue

| Field | Value |
| --- | --- |
| Issue | `shimpeiws/coding-worker-smoke#<N>` |
| `source_revision` (`updatedAt` token) | |
| `content_digest` | |
| Labels | `[]` (verified empty at submit) |
| Body | verbatim `body_template` from `fixtures/s1/seeded-issue.json` |

## Remote references (evidence references, not Core identities)

| Field | Value |
| --- | --- |
| Source branch | `work/w-…` |
| Pull request | `shimpeiws/coding-worker-smoke#<PR>` — state (must be open, never merged) |
| H1 head | `<sha>` |
| H2 head (if fix round ran) | `<sha>` |
| Check runs | per head: `verify`, `review` ids + conclusions |
| Reviews | per head: review id, author (`github-actions[bot]` under Profile R), state, occurrences |
| Target branch | `main` @ `<base sha>` |

## Criterion coverage

Map every checklist row for this variant to a result. Markers:
`pass` (valid evidence linked), `deviation` (evidence exists but
profile/contract differs — say how), `absent` (not exercised — say why),
`invalid` (evidence fails claim linkage — retain with reason, never
count).

| Row | Criterion (short) | Result | Evidence pointer |
| --- | --- | --- | --- |
| S-1…S-8 / A-1…A-4 / R-1…R-9 as applicable | | | |

## Zero-finding check (mandatory)

Applies to every variant that ran a review leg — `success` requires
findings on H1 **and** an exercised fix round; `Attention` requires
findings on each judged head (the pauses are finding-driven); `recovery`
applies it to whichever review legs actually ran (`n/a` only when no
review leg ran, with the reason stated).

- [ ] H1 review round occurrence count: `<n>`
- [ ] If `n = 0` on a review leg that needed findings: mark
      `review/fix-unexercised`; this run does **not** satisfy the
      review/fix leg; a new run tag and seed are required.
- [ ] Fix round (if any) produced a distinct head H2 and a fresh review.

## Incidents inside the run

Recorded, not hidden: abandoned attempts, injected faults and their
arming evidence, worker failures, rejected commands (`stale_work_version`,
`idempotency_conflict`), `unknown` envelopes and their recovery lookups.

## Classification

- [ ] **live** — real Worker process, real GitHub reads/writes, real
      store; executed under recorded owner authorization.
- [ ] **Profile R** — `s1-bridge` adapter, `github-actions[bot]`
      reviewer, `default` credential reference, `host_write: true`,
      deterministic scenario Worker. Not target-profile evidence.
- [ ] Fixture/contract results cited (if any) are labeled fixture and
      support contract claims only.

## Owner approval

| Field | Value |
| --- | --- |
| Evidence approved by | `[owner: name]` |
| Approved at (UTC) | `[owner: timestamp]` |
| Approval record | `[owner: #113 comment permalink]` |
| Deviations accepted | `[owner: list or "none"]` |
