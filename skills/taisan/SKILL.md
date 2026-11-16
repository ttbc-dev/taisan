---
name: taisan
description: Operate the taisan CLI (execution loop) for an agentic workflow — submit Work, inspect recorded state and Attention, intervene on eligible Executions, judge review findings, and recover lost command responses. Install with `npx skills add shimpeiws/taisan`.
---

# taisan — run the execution loop through the CLI

You are operating the documented `taisan` CLI envelope. This skill is a thin
wrapper: every operation below is one-to-one with a documented Command or
Query, each delegates to the matching `taisan` invocation, and nothing in
this skill adds, removes, or reinterprets any capability. If you ran the
identical direct CLI calls instead of this skill, you would get the same
behavior, the same replay identity, and the same envelopes.

## Non-negotiable rules

1. **Replay identity is preserved, never rewritten.** Before any mutation,
   retain `(origin, actor, idempotency_key)`, the exact payload, and the
   displayed preconditions (`--work` + `--expected-version`,
   `--expected-execution`, `--head`, Attention version). A rejection is a
   recorded decision bound to that identity — never retarget it.
2. **Never rewrite preconditions to make a rejection pass.** Do not bump
   `--expected-version`, swap a `--head`, rebind a finding decision, or
   mint a fresh key to get past `rejected` / `unknown`. A fresh key is a
   new Command, not a recovery.
3. **Unknown results recover through reconciliation only.** If a mutation's
   fate is `unknown` (lost response), recover with
   `taisan command-result` for the original `(origin, actor,
   idempotency_key)`, or replay the byte-identical command. Never issue a
   fresh key or an edited payload for the same operation.
4. **The skill carries no execution policy.** It never maintains a queue,
   never launches writers outside `taisan`, never invents retry or recovery
   policy, and never treats a conversation ending as cancellation.
   Recorded Work and its ownership remain inspectable after the session
   ends; closing this session cancels nothing.
5. **`process exit 0` is not evidence of acceptance.** Read the envelope:
   `status` ∈ `accepted | rejected | unknown | error` and the `reason.code`
   for rejections/errors. Diagnostics on stderr never corrupt the JSON on
   stdout; commands print exactly one JSON object.
6. **Direct CLI fallback is always available.** Every operation below is
   runnable as the identical direct `taisan ...` call when this skill is
   not installed. Do not gate any step on the skill being present.
7. Acceptance is never Work completion. "Accepted" means durably queued or
   the Intervention applied — always verify with Queries before claiming
   an outcome.

## Before you mutate

1. Read the current recorded state: `work-list`, then `work-detail` for
   the Work, `attention` for what needs an actor, `capacity` for headroom.
2. Take identities and preconditions from that output — never from memory.
3. Choose an idempotency key and keep it. Reuse the same key to replay the
   same Command.

## Setup the installation (operator)

Setup configures the installation profile; it runs before any workflow
Operation and never touches queued Work. The same command lines run
verbatim as direct CLI calls. Flags below are the minimal shapes; each
takes repeatable forms documented by the CLI's own help output.

```
taisan init --endpoint <ENDPOINT> --repo <REPO_ID:USE> --role <ROLE:ACTOR:CREDENTIAL_REF> --observe <ROLE:LOGIN@HOST/REPO> --egress <EGRESS_HOST>
```

```
taisan check
```

```
taisan check --probe
```

`init` scaffolds the profile from observed evidence — it never
fabricates identity and never rewrites an existing config. `check` is
the ordered offline preflight: exit 0 when every stage passes, exit 3
at the first failing stage. `check --probe` is opt-in and fails closed
with `capability.unknown` (exit 3) while no probe backend is wired — a
refusal, never a fabricated pass. Setup commands are not replayable
mutations: their envelopes carry `command: null`, and they queue
nothing.

## Operations

Each block is the minimal runnable invocation; the same command lines
work verbatim as direct CLI calls. Optional flags are listed in the
prose under each block. Placeholders: `<WORK_ID>`, `<EXECUTION_ID>`,
`<N>` (observed `work-detail` version), `<KEY>` (your idempotency key).

### Submit new Work

```
taisan submit --intent <INTENT> --idempotency-key <KEY>
```

Accepted means durably queued — not completed, not necessarily started.
Optional: `--issue <repo>:<number>` (repeatable, each paired positionally
with `--issue-revision <rev>` and `--issue-digest <digest>`; the CLI never
fabricates them), `--constraint "<constraint>"` (repeatable),
`--verification "<argv> <argv> ..."` (repeatable, space-separated).

### Inspect progress and evidence (Queries)

```
taisan work-list
```

```
taisan work-detail --work <WORK_ID>
```

```
taisan attention --work <WORK_ID>
```

```
taisan capacity
```

`attention` without `--work` spans every Work. Show canonical identities,
recorded state, unavailable sources, and blockers. Agent inference is not
recorded evidence; an unavailable source is surfaced in
`sources[].availability` inside an `accepted` envelope, never translated
into an empty workload. A not-found item is `accepted` with
`result: null`; an unreadable store is `error` at exit 4. (Exit 5 is not
produced by Queries — it belongs to a `rejected` Command whose
`reason.code` is `source_unavailable`.)

### Intervene on eligible Executions

```
taisan retry --work <WORK_ID> --expected-version <N> --expected-execution <EXECUTION_ID> --idempotency-key <KEY>
```

```
taisan resume --work <WORK_ID> --expected-version <N> --execution <EXECUTION_ID> --idempotency-key <KEY>
```

```
taisan cancel --work <WORK_ID> --expected-version <N> --idempotency-key <KEY>
```

```
taisan acknowledge --work <WORK_ID> --expected-version <N> --attention <ATTENTION_ID> --idempotency-key <KEY>
```

Use the observed version and target identities from the Queries above.
`retry` creates a new Execution after confirmed failure; `resume`
continues a paused one; `cancel` is an intervention, not writer-safety
proof: a queued or paused Execution is cancelled immediately, while a
live one settles through the uncertain-stop path until writer safety is
established — cancellation is never stop proof. `acknowledge` applies
only to the currently derivable Attention version and releases nothing.
`cancel` optionally takes `--execution <EXECUTION_ID>`.
Ineligible or stale preconditions are `rejected` at exit 1 (`reason.code`
such as `ineligible` / `stale_work_version`), never silently retargeted.

### Judge review findings

```
taisan judge-findings --work <WORK_ID> --expected-version <N> --head <HEAD> --decision <OCCURRENCE_ID>:fix --idempotency-key <KEY>
```

```
taisan release-judgment --work <WORK_ID> --expected-version <N> --head <HEAD> --idempotency-key <KEY>
```

Preserve head and finding identities. `--decision` is repeatable and
takes `<occurrence_id>:fix|dismiss|accept|wait[:rationale]`; the Core
enforces current finding-set coverage — a stale head or an incomplete or
grown finding set is `rejected` (`stale_head`, `invalid_request`).
Conversational approval alone authorizes nothing.

### Recover a lost command response

```
taisan command-result --origin <ORIGIN> --replay-key <KEY>
```

```
taisan command-result --command-id <COMMAND_ID>
```

Reuse the original `(origin, actor, idempotency_key)` — or replay the
byte-identical command. If recovery is unavailable, preserve the
`unknown` status and do not create a fresh request.

## Reading results

| `status` | meaning | what you do |
| --- | --- | --- |
| `accepted` | Command/Query completed and its durable result is recorded | continue; acceptance ≠ completion |
| `rejected` | domain refusal; exit 1 (or 5 when `reason.code` is `source_unavailable` — the failure class wins over the rejection class) | read `reason.code`; surface it; never rewrite preconditions |
| `unknown` | fate undetermined (exit 6) | recover via `command-result` / identical replay only |
| `error` | usage/config/store/query-refusal failure; nothing recorded as accepted | fix the environment or input; exit 2 (usage), 3 (config), 4 (store), or 1 (`forbidden`) |

## Scope

Installation is implicit (`load_or_create` on first use) and the
`run` driver, the `propose` selector batch, and resource
claim/release are operator-facing surfaces invoked directly against an
installation; this skill covers the Setup rows and the workflow
Operations listed above and adds nothing else, and it names no surface
that does not ship in the CLI. TUI and MCP are not part of this skill.

## What this skill never does

- It never launches workers outside `taisan`, never maintains its own
  queue or authoritative state, and never invents recovery policy.
- It never retries with a fresh idempotency key under an `unknown` or
  lost-response outcome.
- It never reports an accepted Command as Work completion or as merge
  approval.
- It never acts on conversational approval; judgments bind to recorded
  head and finding identities.
