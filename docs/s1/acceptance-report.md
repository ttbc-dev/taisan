# S1 acceptance report — criteria × variant mapped to evidence

Status: the U8.5 report. Every cell names either a valid evidence claim
(`C-nn`, defined below with full linkage) or an explicit marker:

- `absent` — no evidence was collected on this revision;
- `invalid` — evidence exists but fails the claim linkage (a missing
  required field, or the evidence does not satisfy the criterion as
  written; the reason is recorded inline, never silently dropped).

No cell is left unmarked. This report contains **no U6 (alternate
admission) or U9 (host exchangeability) evidence rows** — those are
separate acceptance tracks; see [evidence-checklist.md](evidence-checklist.md#boundaries).

> **Private references.** `shimpeiws/coding-worker-smoke` is a private
> repository — every link and `#N` citation to it in this report
> resolves only for the owner (they 404 publicly). The name is retained
> verbatim under the owner publication decision (#438).

`config_digest` is `sha256:<hex>` of the sanitized installation config
file (`installation.json`). The valid claims in this report cite U8.2/U8.3
evidence; their digests were taken from the retained files **at the time
this report was written**, not captured at run time, because those
records predate this report's digest definition. That binding is weaker
than a run-time digest and is recorded here rather than papered over.
`scripts/check_s1_acceptance.py` cross-checks each revision SHA against
the linked evidence file and requires a consistent digest per source,
but it cannot recompute the digest: no retained `installation.json` is
in this repo.
The U8.4 recovery slice supplies no claim block here — its outcomes
appear only in the absent/invalid reasons of the table.

| # | Criterion | Variant | Result |
| --- | --- | --- | --- |
| S-1 | submit accepted; W1/E1 queued; no Invocation | success | valid — C-01 |
| S-2 | dispatch admission (slot + reservations + Invocation request atomically) | success | valid — C-02 |
| S-3 | artifact produced; `verification_argv` passes | success | valid — C-03 |
| S-3a | invariant-check Invocation on the approved set before publication | success | invalid — no separate invariant-check Invocation exists on this revision; only u8.2 records the invariant content as named Assessment facts (`s1.writable-paths-bounded` + `s1.no-force-push`), while u8.3 describes it generically (bounded writable paths, no-force-push ancestry) and u8.4 records only a generic invariant-check Assessment fact — no criteria row has a separate Invocation identity |
| S-4 | stop proof → push receipt → open receipt → ChangeHead H1 | success | valid — C-04 |
| S-5 | real checks and a real review (≥1 finding) on H1 | success | invalid — H1 evidence is real (checks success; review `5836107428`, 2 occurrences) but the reviewer identity was `github-actions[bot]`, not the dedicated S1 reviewer GitHub App the installation spec requires; claim identity fails linkage |
| S-6 | judge(expected_head=H1) → fix → I2 → republish H2 | success | valid — C-05 |
| S-7 | reassessment on H2 (verification + checks + clean fresh review) | success | valid — C-06 |
| S-8 | `work` records `succeeded` | success | invalid — u8.2's Outcome froze 8 evidence refs (spec, target, both artifacts, all three applied actions, clean round); no writer-stop record is among them, and the criterion requires writer-stop evidence in the Outcome. Invocation exits (inv-2/inv-3) are recorded as observations but are not frozen into the Outcome — linkage fails |
| A-1 | finding → `paused(human_judgment)` + Attention | attention | valid — C-08 |
| A-2 | Attention visible until acknowledged; ack is presentation-only | attention | valid — C-09 |
| A-3 | judge → release → requeue; budgets unchanged | attention | valid — C-10 |
| A-4 | `no_progress` threshold | attention | absent — never exercised on this revision; u8.3 records last material progress ~57 s after dispatch, far inside `no_progress_seconds 1800`; nothing to link |
| R-1 | duplicate submit (same key + payload) replays the original decision | recovery | absent — never exercised live; covered only by store/fault-matrix contract tests |
| R-2 | duplicate key, changed payload → `idempotency_conflict` | recovery | absent — never exercised live |
| R-3 | lost command response recovered via `command-result` | recovery | absent — never exercised live (u8.2/u8.3/u8.4 used only fresh idempotency keys) |
| R-4 | publication result unknown (push/PR-create response lost); read-back reconciliation, no second write | recovery | invalid — U8.4 fault (a) did exercise a lost push response, but on this revision the original action's receipt was never recorded (`pending`, not `unknown`), and the bridge re-ran `git push` (an exact-lease no-op) instead of settling solely from a read-back; the R-4 checklist row's required evidence does not exist, so the claim fails linkage. fault (b) corroborates "no duplicate effect" (zero PRs) but is a definite failure, not a lost response |
| R-5 | installation restart while an Invocation may be live | recovery | absent — fault (a) and fault (c) of U8.4 exercised restart with an unverified *host-effect intent* and a pending *RetryIntent*, respectively; the possibly-live *worker process* leg (unverified Invocation, PID/start-time re-verification, descendant inspection) remains unexercised on this revision |
| R-6 | stale review / stale head rejected (`stale_head`) | recovery | absent — never exercised; recorded `stale_work_version` rejections are not the stale-head criterion |
| R-7 | GitHub adapter outage; `unavailable` Observations; sources distinguish "no findings" from "source unavailable" | recovery | absent — never exercised |
| R-8 | budget exhaustion → stopping/uncertain → `failed` + limit code; `retry` creates E2 claiming the same workspace/branch | recovery | invalid — u8.2's `e-1` failed with `artifact_recovery_exhausted` (a shim syntax fault), not with `max_invocations`/`max_review_rounds`/`runtime_limit_seconds` exhaustion; the `retry` leg (cmd-3 → e-2, same workspace) is real but was driven by artifact failure, not budget, so the R-8 claim fails the linkage its criterion demands |
| R-9 | duplicate Observation delivery deduplicated | recovery | valid — C-13 |

## Claims

All revisions below are on `main`-descendant branches of the evidence
PRs; each claim names its source evidence file and the exact recorded
identities. `stop_proof` names what the supervisor recorded; `null`
only with a reason.

### C-01 (S-1, success)
```json
{
  "id": "C-01",
  "criterion": "S-1",
  "variant": "success",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-1", "e-2"],
    "invocations": [],
    "actions": [],
    "commands": ["cmd-1", "cmd-3"]
  },
  "remote_receipts": {
    "pr": null,
    "pr_reason": "no PR exists at submit time; PR #14 was opened later in the same run (see C-04)",
    "head": null,
    "head_reason": "no head exists at submit time; first published head was e01bf9ad12d99967e42f4e00063d94476b944468 (see C-04)",
    "checks": [],
    "checks_reason": "no check exists at submit time; checks recorded for this Work: see C-04/C-06",
    "review": null,
    "review_reason": "no review exists at submit time; reviews recorded for this Work: see C-04/C-06"
  },
  "stop_proof": null,
  "stop_proof_reason": "no writer exists at submit time; writer stop proofs for this Work: see C-04 (inv-2) and C-05 (inv-3)"
}
```

### C-02 (S-2, success)
```json
{
  "id": "C-02",
  "criterion": "S-2",
  "variant": "success",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-1", "e-2"],
    "invocations": ["inv-1", "inv-2", "inv-3"],
    "actions": [],
    "commands": ["cmd-1", "cmd-3"]
  },
  "remote_receipts": {
    "pr": null,
    "pr_reason": "no PR exists at dispatch time; PR #14 was opened later in the same run (see C-04)",
    "head": null,
    "head_reason": "no head exists at dispatch time; see C-04 for the published head",
    "checks": [],
    "checks_reason": "no check exists at dispatch time; see C-04/C-06",
    "review": null,
    "review_reason": "no review exists at dispatch time; see C-04/C-06"
  },
  "stop_proof": null,
  "stop_proof_reason": "no writer completes at dispatch time; stop proofs: see C-04/C-05"
}
```

### C-03 (S-3, success)
```json
{
  "id": "C-03",
  "criterion": "S-3",
  "variant": "success",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-2"],
    "invocations": ["inv-2"],
    "actions": [],
    "commands": ["cmd-3"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#14",
    "head": "e01bf9ad12d99967e42f4e00063d94476b944468",
    "checks": [
      {"name": "verify", "id": "129597981958", "conclusion": "success"},
      {"name": "review", "id": "129597977975", "conclusion": "success"}
    ],
    "review": null,
    "review_reason": "review arrives after H1 publish; see C-04 for the H1 review"
  },
  "stop_proof": "inv-2 Exited with verified stop recorded by the supervisor (artifact bound to the produced commit; process exit recorded)"
}
```

### C-04 (S-4, success)
```json
{
  "id": "C-04",
  "criterion": "S-4",
  "variant": "success",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-2"],
    "invocations": ["inv-2"],
    "actions": ["act-19f925f6", "act-57ebdebe"],
    "commands": ["cmd-3"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#14",
    "head": "e01bf9ad12d99967e42f4e00063d94476b944468",
    "checks": [
      {"name": "verify", "id": "129597981958", "conclusion": "success"},
      {"name": "review", "id": "129597977975", "conclusion": "success"}
    ],
    "review": {"id": "5836107428", "author": "github-actions[bot]", "state": "COMMENTED"}
  },
  "stop_proof": "inv-2 Exited with verified stop (recorded by the supervisor before publish; stop evidence bound to the published head)"
}
```

### C-05 (S-6, success)
```json
{
  "id": "C-05",
  "criterion": "S-6",
  "variant": "success",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-2"],
    "invocations": ["inv-3"],
    "actions": ["act-f73dd542"],
    "commands": ["cmd-4", "cmd-5"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#14",
    "head": "f54c97bd01995e9606faf4d29c0536d9a7a5b4cd",
    "checks": [
      {"name": "verify", "id": "129599695802", "conclusion": "success"},
      {"name": "review", "id": "129599695262", "conclusion": "success"}
    ],
    "review": {"id": "5836140631", "author": "github-actions[bot]", "state": "COMMENTED"}
  },
  "stop_proof": "inv-3 Exited with verified stop (republish leg re-armed on the new artifact digest; push receipt records prior ref H1)"
}
```

### C-06 (S-7, success)
```json
{
  "id": "C-06",
  "criterion": "S-7",
  "variant": "success",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-2"],
    "invocations": ["inv-3"],
    "actions": ["act-f73dd542"],
    "commands": ["cmd-4", "cmd-5"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#14",
    "head": "f54c97bd01995e9606faf4d29c0536d9a7a5b4cd",
    "checks": [
      {"name": "verify", "id": "129599695802", "conclusion": "success"},
      {"name": "review", "id": "129599695262", "conclusion": "success"}
    ],
    "review": {"id": "5836140631", "author": "github-actions[bot]", "state": "COMMENTED", "occurrences": 0}
  },
  "stop_proof": "inv-3 Exited with verified stop (H2 bound to the fixed artifact; reassessment on H2 recorded)"
}
```

### C-08 (A-1, attention)
```json
{
  "id": "C-08",
  "criterion": "A-1",
  "variant": "attention",
  "status": "valid",
  "source": "docs/evidence/u8.3.md#acceptance-checklist-mapping",
  "revision": "cb376b7",
  "config_digest": "sha256:70207163902d195fd82f3d29f63085177a7fa022dff0aa1293e166e1420640f1",
  "identities": {
    "work": "w-2",
    "executions": ["e-2"],
    "invocations": ["inv-1", "inv-2"],
    "actions": ["act-d90ef975", "act-14eb2e18", "act-0c69d185"],
    "commands": ["cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#16",
    "head": "397deda4780336da73fdc9cb90cd21bfc47f1fd4",
    "checks": [
      {"name": "verify", "id": "129636315608", "conclusion": "success"},
      {"name": "verify", "id": "129636334371", "conclusion": "success"},
      {"name": "review", "id": "129636337848", "conclusion": "success"}
    ],
    "review": {"id": "5836865877", "author": "github-actions[bot]", "state": "COMMENTED"}
  },
  "stop_proof": "inv-1/inv-2 Exited with verified stop; the halt itself followed the Work-wide max_fix_invocations bound, recorded as attention att:w-2:paused_judgment:Work-wide fix invocation bound reached; human action required:e-2:157:32"
}
```

### C-09 (A-2, attention)
```json
{
  "id": "C-09",
  "criterion": "A-2",
  "variant": "attention",
  "status": "valid",
  "source": "docs/evidence/u8.3.md#acceptance-checklist-mapping",
  "revision": "cb376b7",
  "config_digest": "sha256:70207163902d195fd82f3d29f63085177a7fa022dff0aa1293e166e1420640f1",
  "identities": {
    "work": "w-2",
    "executions": ["e-2"],
    "invocations": [],
    "actions": [],
    "commands": ["cmd-8"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#16",
    "head": "397deda4780336da73fdc9cb90cd21bfc47f1fd4",
    "checks": [
      {"name": "review", "id": "129636337848", "conclusion": "success"}
    ],
    "review": {"id": "5836865877", "author": "github-actions[bot]", "state": "COMMENTED"}
  },
  "stop_proof": null,
  "stop_proof_reason": "no writer to stop at the ack step; writer stop proofs: see C-08"
}
```

### C-10 (A-3, attention)
```json
{
  "id": "C-10",
  "criterion": "A-3",
  "variant": "attention",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-2"],
    "invocations": ["inv-3"],
    "actions": ["act-f73dd542"],
    "commands": ["cmd-4", "cmd-5"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#14",
    "head": "f54c97bd01995e9606faf4d29c0536d9a7a5b4cd",
    "checks": [
      {"name": "verify", "id": "129599695802", "conclusion": "success"}
    ],
    "review": {"id": "5836140631", "author": "github-actions[bot]", "state": "COMMENTED", "occurrences": 0}
  },
  "stop_proof": "inv-3 Exited with verified stop; fix_invocations_used was charged exactly once for inv-3 (the Fix invocation) and not reset by either pause (u8.3 records the same for its inv-2)"
}
```

### C-13 (R-9, recovery)
```json
{
  "id": "C-13",
  "criterion": "R-9",
  "variant": "recovery",
  "status": "valid",
  "source": "docs/evidence/u8.2.md#acceptance-checklist-mapping",
  "revision": "8009e8b",
  "config_digest": "sha256:10616c4cdb385b09dd1ea4d8c93b75ab9f9338abe4140136e2c539b4e0b52828",
  "identities": {
    "work": "w-1",
    "executions": ["e-2"],
    "invocations": [],
    "actions": ["act-19f925f6"],
    "commands": ["cmd-1"]
  },
  "remote_receipts": {
    "pr": "shimpeiws/coding-worker-smoke#14",
    "head": "e01bf9ad12d99967e42f4e00063d94476b944468",
    "checks": [
      {"name": "verify", "id": "129597981958", "conclusion": "success"}
    ],
    "review": {"id": "5836107428", "author": "github-actions[bot]", "state": "COMMENTED"}
  },
  "stop_proof": null,
  "stop_proof_reason": "duplicate deliveries were inert (recorded Duplicate, no second intent/effect); no writer is involved in a re-delivered observation; writer stop proofs: see C-04"
}
```

## Linkage audit trail

- `docs/evidence/u8.2.md` — U8.2 live success run (e-1 failed
  `artifact_recovery_exhausted`; e-2 `succeeded`), recorded 2026-11-09/10
  by the owner session.
- `docs/evidence/u8.3.md` — U8.3 live Attention run (`e-2` paused on the
  Work-wide fix bound), recorded 2026-11-10.
- `docs/evidence/u8.4.md` — U8.4 live recovery-variant runs, recorded
  2026-11-11 (this slice).
- Revisions: `8009e8b` (U8.2 tested), `cb376b7` (U8.3 tested), `03d1513`
  (U8.4 fault hooks).
- All claims are fixture-of-live: every cited number exists in the
  linked evidence file; nothing here is re-derived from a re-run.
- Known deviations carried forward: S-3a (Assessment-fact invariant
  leg), S-5 (reviewer identity `github-actions[bot]`), R-5
  (possibly-live-worker leg), A-4 (no_progress), R-1/R-2/R-3/R-6/R-7
  (never exercised). Each is marked `invalid`/`absent` in the table
  above rather than absorbed into a "pass".
