# Follow-up: reuse existing diagnostics for Retro; keep Wayfinder upstream

Status: **Proposal amendment — documentation only**  
Related: [Wayfinder / Retro / Ratchet proposal](proposal-wayfinder-retro-ratchet.md), [PR #193](https://github.com/shimpeiws/taisan/pull/193).

## Why this follow-up exists

The original proposal describes a read-only Retro CLI as the first experiment. That wording risks duplicating existing diagnostic and lane-recovery work in [hermes-setup](https://github.com/shimpeiws/hermes-setup). It also describes Wayfinder as selector-side planning without explicitly accounting for the existing **devflow** planning responsibility.

**Revised recommendation:** audit and reuse existing diagnostics first. Treat Retro as a **cross-run learning and verification capability**, not a new diagnostic command by default. Treat Wayfinder as an optional *upstream devflow-like decision workflow*, not a taisan Core feature.

This document supplements, rather than silently overwrites, the merged proposal. It changes no runtime behavior.

## What is actually present in hermes-setup

The [hermes-setup README](https://github.com/shimpeiws/hermes-setup/blob/main/README.md) describes:

- Coding-worker lane dispatch, ship/converge and bounded recovery.
- Durable resume and shadow-review events; PR binding provenance and typed dropped/quarantined dispositions.
- Finding-disposition reconciliation keyed by repository, PR, finding and commit.
- Halt episodes, judgment waits, decision bundles and operator recovery flows.
- Several narrowly scoped diagnostic/maintenance scripts and scheduled sweeps.

These are promising **evidence sources and diagnostic precedents**, not proof that a reusable Retro API exists.

**Unverified:** the user-mentioned *lane doctor* skill/script could not be located at the obvious `skills/lane-doctor/SKILL.md`, `docs/lane-doctor.md`, or `scripts/lane-doctor.py` paths. It may have a different name, live on a branch, or be part of another skill. Its exact overlap and code reuse cannot be asserted until located. Do not create a duplicate tool before that audit.

## Clarify the responsibilities

| Concern | Primary home | Purpose | Output |
| --- | --- | --- | --- |
| Decision discovery (Wayfinder) | Upstream planning / devflow-like workflow | Resolve unknowns before Work acceptance | Decision map, rationale, implementation-ready spec |
| Run diagnostics (lane doctor and existing scripts) | Operational tooling around hermes-setup / future taisan | Determine what happened and what can safely resume **now** | Evidence-backed diagnosis, safe recovery recommendation |
| Durable execution and reconciliation | taisan Core + adapters | Maintain authoritative Work/Execution/Invocation state | Observations, outcomes, attention and evidence |
| Retro learning | Separate analysis *workflow* using existing diagnostics | Find recurring failure patterns across runs and propose prevention | Improvement hypothesis and reviewable change proposal |
| Improvement verification | Tests / Trace2Eval / measured production outcomes | Test whether the proposed fix actually reduces recurrence | Reproducible regression evidence and KPI delta |
| Ratchet | Repository checks and CI | Prevent new violations and reduce an approved legacy baseline | Versioned check results and shrinking exceptions |

**Key distinction:** diagnosis is about the *current incident*; Retro is about *recurrent causes and verified future prevention*. The two should share evidence parsers and failure taxonomy, rather than necessarily share an executable or process.

## Architecture preference

```text
Existing hermes-setup diagnostics + lane doctor (audit exact implementation)
                  |
                  v
          Shared evidence normalization
                  |                     taisan durable observations
                  +-------------------------------+
                                                  |
                                         Cross-run Retro analysis
                                                  |
                                improvement proposal / reviewed PR
                                                  |
                                  tests / Trace2Eval / KPI comparison
                                                  |
                                 approved changes to future Factory runs
```

A reusable library, an extension to an existing script, or a skill over existing commands are all acceptable. A new standalone CLI is **not** an architectural requirement. Do not migrate Hermes-specific code wholesale into taisan Core; isolate portable evidence schemas/classifiers from operational adapters.

## Required source audit before implementation

1. **Locate lane doctor precisely:** skill path, invocation, branch/PR, inputs, output schema and whether it is read-only or can take recovery actions.
2. **Inventory existing scripts:** identify overlap in failure classification, run log parsing, halt/judgment episode extraction, finding reconciliation and provenance checking. Record file paths and call sites.
3. **Trace evidence availability:** which fields are actually persisted/exportable in hermes-setup versus the implemented taisan Rust slice and which remain target design only.
4. **Classify portability:** generic evidence/diagnosis, Hermes-specific control, and source-workflow compatibility; explicitly avoid two independent truth sources.
5. **Select the smallest integration:** reuse a skill or library if possible; only introduce a new CLI if there is a concrete missing entrypoint.
6. **Evaluate on historical incidents:** compare the candidate Retro output against existing lane doctor diagnoses, known human decisions and later fixes.

### Proposed first vertical slice

Using **three historical incidents** (one recovered halt, one repeated review/finding failure, one uncertain publication or PR binding), produce a read-only comparison:

- What did existing diagnostics already identify?
- What additional **cross-run** insight did Retro produce?
- Which improvement is testable by a regression case or Trace2Eval?
- What would count as fewer **avoidable** human interventions on a comparable workload?
- What is missing or unknown in the evidence?

Exit only if Retro adds a falsifiable insight not already produced by incident diagnosis. Otherwise extend existing diagnostics instead of launching a new component.

## Impact on the merged proposal

- **Wayfinder:** replace “selector-side new feature” with “upstream devflow-like decision mapping, with approved handoff into Candidate/Work.” No generic planning state in Core.
- **Retro:** replace “independent CLI” with “reuse existing diagnostics and add cross-run improvement analysis where a gap is proven.” Do not assume lane doctor is already verified.
- **Ratchet:** unchanged; keep deterministic versioned checks in repository CI and never use exception baselines to waive Work invariants.
- **Priorities:** source audit → minimal Retro experiment → Ratchet pilot → optional Wayfinder experiment. No v0.1 release gate.
- **Governance:** improvements to Factory prompts, policy or code require independent review and evidence-backed evaluation; no autonomous self-approval.

## Acceptance criteria for a subsequent implementation proposal

- [ ] Lane doctor implementation located and reviewed at exact source paths.
- [ ] Existing diagnostic scripts and persisted evidence fields inventoried with code references.
- [ ] Explicit reuse-vs-new-component decision with rationale.
- [ ] Three historical incidents compared, including unknown/insufficient-evidence cases.
- [ ] One proposed improvement linked to reproducible verification.
- [ ] No expansion of taisan Core execution authority or v0.1 scope.
