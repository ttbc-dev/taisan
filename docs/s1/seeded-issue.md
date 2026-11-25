# S1 seeded issue definition

Status: the deterministic fixture that produces the single input issue
for one S1 run. Creating the seed is a fixture procedure, not live
execution — the run itself is #113.

> **Private references.** `shimpeiws/coding-worker-smoke` is a private
> repository — every link and `#N` citation to it in this document
> resolves only for the owner (they 404 publicly). The name is retained
> verbatim under the owner publication decision (#438).

## Requirements

A seeded issue must satisfy all of:

| Requirement | Rationale |
| --- | --- |
| Small, deterministic change request | The expected diff is known and independently checkable; the run's verification does not depend on judgment |
| Independently checkable result | A stated `verification_argv` (the test repository's real verification command) decides pass/fail against the produced artifact |
| Engineered review-visible gap | The issue leaves at least one gap a real reviewer flags (e.g., a missing edge-case guard the request text explicitly permits but best practice requires) — a zero-finding review leaves the review/fix leg unexercised |
| No automation-selected labels | The issue carries **no label any automation lane selects on**; it is created manually or by the fixture procedure, never through a lane's intake |
| Fresh identity per run | Each run seeds a new issue; issue numbers and seeded content are evidence references, never replayed across runs |

## Seeded issue template

| Field | Content |
| --- | --- |
| Title | `S1 seed <run-tag>: normalize greeting output` |
| Body | Asks the worker to add a `greet(name)` helper to the test repository's sample module that returns `Hello, <name>!` for non-empty trimmed input and `Hello!` for empty/whitespace input; the request **does not** mention input validation of `None`/`null` callers or a docstring — the engineered review-visible gap |
| Constraints recorded at submit | Writable paths limited to the sample module and its test file; force-push forbidden; no merge/deploy authority |
| `verification_argv` | The test repository's real verification command (e.g., its test runner against the module's test file); recorded verbatim in the submit payload |
| Labels | **None** — see the label rule below |
| Expected review gap | At least one finding is expected (missing docstring and/or unguarded `None` input); a review returning zero findings invalidates the run for the review/fix leg |

## Label rule

Seeded issues must not carry any label an automation lane selects on.
Before each run, the procedure verifies the seeded issue's label set is
empty or disjoint from every lane-selection label in the target
repository. Because `shimpeiws/coding-worker-smoke` was removed from
the worker automation lane's repository allowlist (2026-11-09), lane
intake never observes the seed; the label rule is the second,
structural guarantee.

## Fixture procedure

1. **Record the run tag**: a unique `<run-tag>` (UTC timestamp + short
   random suffix) names this seed and appears in the title, the run's
   evidence record, and — indirectly — the `work/<work_id>` branch.
2. **Create the issue** in `shimpeiws/coding-worker-smoke` using the
   template above, with **no labels** (and none added later by the
   procedure). Record the issue number and its content revision/digest
   as evidence references.
3. **Verify prerequisites** per the [preflight](README.md#prerequisite-verification-pre-live-preflight):
   allowlist, credential reference, reviewer identity, worker identity,
   store initialized — none of which authorizes execution.
4. **Submit** through `taisan submit` with the issue reference, the
   stated constraints, `verification_argv`, and a recorded
   `(origin, actor, idempotency_key)`. The recorded Command identity
   and the resulting `work_id`/`execution_id` enter the run's evidence
   record.
5. **Reseeding**: if a seeded issue is consumed, closed, or polluted
   (labels applied, manual edits), the procedure creates a fresh issue
   under a new run tag; the polluted seed is retained as evidence and
   never reused.

## What this fixture does not do

- It does not create PRs, push branches, or post comments — only the
  worker and the GitHub adapter produce remote effects, under their
  recorded identities.
- It does not bypass admission: the seeded issue enters through the
  ordinary `submit` Command with full preconditions.
- It does not authorize a zero-finding pass as success — see the
  [zero-finding rule](evidence-checklist.md#zero-finding-rule).
