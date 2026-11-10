# S1 criterion-to-evidence checklist

Status: the evidence contract for one S1 run. Each row names what must
be **recorded** (identities, receipts, observations) for a criterion to
count as exercised. Fixture or contract-test results never satisfy
these rows — they require live evidence, and live execution is #113's
separate authorization.

For every run the evidence record carries: taisan revision, the
sanitized installation config identity, the seeded-issue reference, all
produced `command_id`/`work_id`/`execution_id`/`invocation_id`/
`action_id` values, remote references (issue number, PR number, head
SHAs, check/review identities), and per-row results. Credential values
never appear; references appear by name only.

## Success variant

| # | Criterion (from [v0.1 S1](../v0.1.md#expected-path)) | Required recorded evidence |
| --- | --- | --- |
| S-1 | `submit` accepted; `W1`/`E1` `queued` in one commit; no Invocation | CLI envelope `status: accepted` + `command_id`, `work_id`, `execution_id`; store state showing `queued` and no `invocations_for_execution` |
| S-2 | Dispatch admission: capacity slot + write-resource reservation + first Invocation request atomically; `E1 ready` → `I1 (coding) running` | Slot holder record, reservation keys (`workspace` + `(repo, work/<work_id>)`), `InvocationRequest` + `Invocation` identities sharing one commit |
| S-3 | `I1` produces the immutable change artifact; `verification_argv` passes | Invocation result identity, artifact reference + digest, verification argv and exit evidence bound to the artifact |
| S-3a | Invariant-check Invocation passes on the approved set before publication | Separate Invocation identity for the invariant-check role; conformance evidence bound to the same head |
| S-4 | Stop proof → `push_ref` intent → verified ref/revision receipt → separate `open_change_request` → receipt + `change_head` Observation `H1` | Writer-stop evidence (descendant snapshot + inspection); `action_id` + applied receipt for the push; distinct `action_id` + receipt for the create; `change_head` Observation binding `H1` |
| S-5 | Real checks and a real review on `H1` arrive as Observations; review carries ≥ 1 finding | `check` Observations bound to `H1` with named check identities; `review`/`review_findings` Observation bound to `H1` with the reviewer App identity and occurrence set |
| S-6 | `judge_findings(expected_head=H1)` → `fix` → `I2` → stop proof → `push_ref` with prior ref `H1` → verified `H2` | Judgment command identity + recorded round/occurrence bindings; fix Invocation identity + stop proof; push receipt showing prior ref `H1` → `H2` |
| S-7 | Reassessment on `H2`: mechanical verification + invariant check pass; configured CI passes; fresh `H2` review clean | Invocation/artifact/verification evidence bound to `H2`; `check` Observations bound to `H2`; `H2` review evidence with no unresolved finding |
| S-8 | `work` records `succeeded` | Terminal Outcome storing PR reference, verified head `H2`, `H2` artifact, supervisor checks, enabled invariant assessment on `H2`, configured CI, `H2` review evidence, all required effect receipts, writer-stop evidence |

## Attention variant

| # | Criterion | Required recorded evidence |
| --- | --- | --- |
| A-1 | A review finding requiring human judgment per policy → `paused(human_judgment)` | Execution state `paused` with recorded reason; Attention item derived (`paused_judgment`) with `attention_id`, `next_actor`, `next_action` |
| A-2 | Attention remains visible until acknowledged; acknowledgement changes presentation only | `attention` Query before and after `acknowledge` on the exact Attention version; underlying condition unchanged |
| A-3 | `judge_findings` → `release_judgment` → requeue without losing WIP or resetting budgets | Judgment/release command identities; requeue decision record; usage ledger showing consumed budgets unchanged |
| A-4 | `no_progress` threshold exceeded | `no_progress` Attention item carrying Work, Execution, last progress timestamp, elapsed; auto-clear on material progress |

## Recovery variant

| # | Criterion (from [abnormal-input matrix](../v0.1.md#abnormal-input-matrix)) | Required recorded evidence |
| --- | --- | --- |
| R-1 | Duplicate `submit` (same key + payload) | `command_result` returns the original decision; store holds one W/E; second call replays the recorded `command_id` |
| R-2 | Duplicate key, changed payload | `rejected` with `idempotency_conflict`; original retained; no state change |
| R-3 | Lost command response | `command-result` by `(origin, actor, idempotency_key)` or identical replay recovers the decision; no fresh-key duplicate; `unknown` is preserved until recovered |
| R-4 | Publication result unknown (push/PR-create response lost) | `host_action_result: unknown` on the original `action_id`; read-back reconciliation against the recorded intent/binding; separate typed receipts; no second push or PR creation from response loss |
| R-5 | Installation restart while an Invocation may be live | Post-restart reconciliation of the recorded request/process identity; Execution `uncertain` or correctly `running`; no second writer while the old one is unproven; descendant snapshot + PID/start-time re-verification |
| R-6 | Stale review / stale head | Review remains evidence for its own head only; `judge_findings` with `expected_head=H1` after `H2` rejected `stale_head`; a delayed old-head observation never moves current head backward |
| R-7 | GitHub adapter outage | Observations marked `unavailable`; query `sources` distinguish "no findings" from "source unavailable"; no completion claimed; recovery Observations after restore |
| R-8 | Budget exhaustion (`max_invocations`/`max_review_rounds`/`runtime_limit_seconds`) | `stopping`/`uncertain` while writers/effects may act; `failed` with the exhausted limit code once safe; WIP and evidence preserved; `retry` creates `E2` claiming the same workspace/branch with reservation records |
| R-9 | Duplicate Observation delivery (same source event re-delivered) | The redelivered event is inert: one recorded fact, no second intent, no second effect; delivery identities show the duplicate was observed and deduplicated |

## Zero-finding rule

If the review leg returns zero findings, the review/fix path is
unexercised and the run **cannot satisfy S1** — record the run as
review/fix-unexercised, adjust the seed per the
[fixture procedure](seeded-issue.md#fixture-procedure), and rerun under
a new run tag. A zero-finding pass is never reinterpreted as success.

## Boundaries

- S1's `succeeded` means the completion boundary in
  [v0.1](../v0.1.md#completion-boundary) — never merged or deployed;
  merge/deploy are human decisions outside the scenario.
- A head change after `succeeded` does not rewrite the Outcome; the
  readiness projection shows the newer head as unreviewed.
- Materials, fixture results, and contract-test greens support contract
  claims only; they never substitute for live evidence.
- U6 alternate admission (reference-Selector proposal) and U9 host
  exchangeability are separate acceptance items, outside this
  checklist.
