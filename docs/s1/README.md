# S1 scenario materials

Status: preparation materials for the **S1** representative live
acceptance scenario defined in [v0.1](../v0.1.md#representative-live-acceptance-scenario).
This directory contains materials and fixtures only — the isolated
installation/repository specification, the deterministic seeded-issue
definition, and the criterion-to-evidence checklist with success,
Attention, and recovery variants. **Preparing these materials does not
authorize live execution.** Executing the live scenario and approving
live evidence is separately scoped (#113) and requires its own explicit
authorization under the [implementation start gate](../design-closure.md#implementation-start-gate).

## Materials

| Material | Contents |
| --- | --- |
| [installation-spec.md](installation-spec.md) | The isolated installation/repository specification: `shimpeiws/coding-worker-smoke` as the sole allowlisted repository, the scoped credential reference, the dedicated reviewer identity, the Worker runtime identity, and the binding/capability profile S1 requires |
| [seeded-issue.md](seeded-issue.md) | The deterministic seeded-issue definition and the fixture procedure that produces it, including the label rule that keeps seeded inputs out of every automation lane |
| [evidence-checklist.md](evidence-checklist.md) | The criterion-to-evidence checklist: every S1 step and abnormal-input row mapped to the recorded identities and receipts a run must retain, split into success / Attention / recovery variants |

## Machine-readable fixtures and preflight

The prose above is paired with three fixture files that carry the
checkable fields:

| Fixture | Contents |
| --- | --- |
| [`fixtures/s1/installation-spec.json`](../../fixtures/s1/installation-spec.json) | Sole-repository allowlist, credential reference (never a value), reviewer and Worker runtime identities, pinned material revision and configuration identity |
| [`fixtures/s1/seeded-issue.json`](../../fixtures/s1/seeded-issue.json) | Deterministic title/body templates, the empty label set and label policy, verification argv, and the expected outcome for each `success`, `Attention`, and `recovery` variant |
| [`fixtures/s1/evidence-checklist.json`](../../fixtures/s1/evidence-checklist.json) | Every S1 criterion row with its variant assignment, exact revision/config values, Work/Execution/Invocation/action identities, and required evidence |

`python3 scripts/s1/check-prerequisites.py` validates the install-spec
fields (allowlist, credential reference, reviewer identity, revision,
configuration, **capability/required-check/budget content**) and all
three materials, fails closed on any missing or invalid entry, and
never executes the live scenario. Binding identities that only a
read-only host/store interaction can resolve are carried as explicit
unresolved references: the shipped preparation fixture reports
`status: prepared` with `installation_ready: false`, while a fully
resolved install spec reports `status: ok`. Neither result is execution
permission, and neither is live evidence. The preflight also recomputes
the sha256 fixture content digest and fails closed when it no longer
matches the digest recorded in `docs/evidence/u8.1.md`, so editing a
fixture cannot leave the evidence digest silently stale. The retained
scenario-bridge rehearsals (`docs/evidence/u8.2.md`, `u8.3.md`) are
recorded separately from the S1 target profile; their adapter, reviewer,
credential-reference and `host_write` values are cross-checked in their
exact recorded forms against those documents.

## Recorded decisions (owner, 2026-11-09)

- **Isolated test repository**: `shimpeiws/coding-worker-smoke`
  (owner-designated S1 target recorded in the issue's decisions). It was
  removed from the worker automation lane's repository allowlist on
  2026-11-09 so the lanes never pick up seeded issues; the S1
  installation allowlist contains only this repository.
- **Credential**: a fine-grained PAT scoped to
  `shimpeiws/coding-worker-smoke` only (contents RW, pull requests RW,
  issues RW, checks R), recorded in installation config as a credential
  reference — never a value. The reference mechanism permits swapping
  to a scoped GitHub App later without changing this spec.
- **Reviewer identity**: a dedicated S1 reviewer GitHub App (created by
  the owner; installed on `shimpeiws/coding-worker-smoke` only),
  separate from the worker credential.
- **Seeded issue fixture**: seeded issues follow the documented fixture
  procedure and must not carry labels any automation lane selects on.
- **Worker runtime identity**: recorded as the reference Worker
  adapter's configured identity in the same install spec.

## Revision and identity recording

Every prepared run records, before execution is considered:

- **Revisions**: the exact taisan CLI/Core/adapter revisions (commit
  SHAs) under test, plus the revision of these materials.
- **Configuration identity**: the installation config identity — store
  ID, host instance ID, repository stable ID, adapter ID, contract
  version, declared capability set, required checks, allowed effects,
  and budget — sanitized so credential references appear by name only.
- **Runtime identities** the run must produce and correlate: the
  submitted Command identity `(origin, actor, idempotency_key)` and its
  `command_id`, the `work_id`, each `execution_id`, each
  `invocation_id`, and every `action_id` for host effects, alongside
  remote references (issue number, PR number, head SHAs, check and
  review identities) as evidence references — never as Core identity.

## Prerequisite verification (pre-live preflight)

Each row is verifiable *before* any live run and none of them is
permission to execute:

| # | Prerequisite | How verified |
| --- | --- | --- |
| P1 | Installation config parses; host instance, repository, adapter, contract version, and actor bindings all validate | `taisan` config load path (`workloop_config`) |
| P2 | Installation allowlist contains exactly `shimpeiws/coding-worker-smoke` | config inspection against this spec |
| P3 | Publication credential reference resolves by name in installation config; no credential value appears in the repo, config files committed, or evidence exports | config inspection; secrets scan of materials |
| P4 | Reviewer App is installed on `shimpeiws/coding-worker-smoke` only and is a distinct identity from the worker credential | owner record; app installation listing (read-only) |
| P5 | Worker runtime identity matches the reference Worker adapter's configured identity | installation spec ↔ worker adapter config comparison |
| P6 | A freshly seeded issue exists per the fixture procedure and carries no automation-selected label | seeded-issue procedure output |
| P7 | The store is initialized in a fresh installation with the S1 capability profile | `taisan` store open + `capacity` query |

## Variants

S1 is exercised as separately evidenced variants, never collapsed into
one pass/fail claim:

| Variant | Covers | Checklist section |
| --- | --- | --- |
| Success | The expected path: submit → dispatch → artifact → publish → checks/review → fix round → reassessment → `succeeded` | [Success variant](evidence-checklist.md#success-variant) |
| Attention | A paused human-judgment or no-progress condition surfacing actionable Attention without silent settlement | [Attention variant](evidence-checklist.md#attention-variant) |
| Recovery | Lost command response, unknown host effect, and restart reconciliation under the original identities | [Recovery variant](evidence-checklist.md#recovery-variant) |

A zero-finding run does not satisfy the review/fix leg — see the
[zero-finding rule](evidence-checklist.md#zero-finding-rule).
