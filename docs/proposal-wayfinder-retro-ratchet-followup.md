# Follow-up: reuse diagnostic capabilities for Retro; keep Wayfinder upstream

Status: **Proposal amendment — documentation only**  
Related: [Wayfinder / Retro / Ratchet proposal](proposal-wayfinder-retro-ratchet.md), [PR #193](https://github.com/shimpeiws/taisan/pull/193).

## Motivation

The original proposal's “independent Retro CLI” is an implementation option, not a requirement. Before introducing a new executable, audit any existing **diagnostic capabilities available to the project**, including their input/output contracts and reusable failure-classification logic. Do not assume their presence or publish their provenance.

Wayfinder should be treated as an optional **upstream planning responsibility** rather than part of taisan Core.

This document contains only generalized design requirements and publicly shareable project concepts. It intentionally does not identify any private source repository, internal skill, script, path, configuration or incident.

## Responsibilities

| Concern | Responsibility | Output |
| --- | --- | --- |
| Decision discovery | Upstream planning workflow | Decision record, rationale, implementation-ready specification |
| Incident diagnosis | Operational diagnostic capability | Evidence-backed diagnosis and safe recovery recommendation |
| Durable execution | taisan Core and adapters | Observations, outcomes, attention and evidence |
| Retro learning | Cross-run analysis of recorded evidence | Recurrence hypothesis and proposed prevention |
| Improvement verification | Regression tests, reproducible task replay and outcome metrics | Reproducible evidence and measured change |
| Quality ratchet | Versioned repository checks | No new violations and controlled reduction of existing exceptions |

Diagnosis answers **what is wrong in this execution and how to recover safely**. Retro asks **what recurring cause can be prevented and how can we verify that prevention**. Share evidence semantics and classifiers where possible; do not require a second command-line tool.

## Preferred architecture

```text
Existing diagnostic interfaces (if available)  +  taisan observations
                         |
                  Evidence normalization
                         |
                 Cross-run analysis
                         |
           Reviewable improvement proposal
                         |
             Regression / replay evaluation
                         |
          Approved change to future workflows
```

A library, existing command extension, or skill can be used. A standalone CLI should only be introduced if the source audit identifies a concrete missing entrypoint. Core remains authoritative for execution and safety.

## Implementation prerequisites

1. Inventory available diagnostic interfaces and determine whether they are read-only or capable of changing execution state.
2. Identify reusable parsers, classifiers and evidence contracts; document **only generic contracts** in public proposals.
3. Verify which observations are implemented and exportable versus merely specified in the target design.
4. Prevent duplicate truth sources and preserve exact artifact revision and provenance.
5. Compare three **synthetic or explicitly approved, sanitized** incident fixtures: recovered failure, repeated review failure and uncertain publication.
6. Identify a falsifiable improvement hypothesis that can be checked with a regression or replay evaluation.

Do not copy private logs, code excerpts, issue links, branch names, internal identifiers, infrastructure configuration, or private-repository URLs into this repository, PR descriptions, comments or test fixtures. The detailed provenance audit must stay in its authorized environment; publish only generalized conclusions after review.

## Amendments to the merged proposal

- **Wayfinder:** upstream planning and decision mapping; approved handoff to Candidate/Work. No planning state machine in Core.
- **Retro:** reuse existing diagnostic capabilities where demonstrably available; add cross-run analysis only for proven gaps. No mandatory new CLI.
- **Ratchet:** unchanged; deterministic checks in CI, with explicit versioned exceptions that never waive Work invariants.
- **Sequence:** capability audit → read-only Retro experiment → Ratchet pilot → optional Wayfinder pilot.
- **Authority:** Factory changes require independent review and evidence-based evaluation; no autonomous self-approval.

## Acceptance criteria

- [ ] Diagnostic reuse decision is grounded in an authorized source audit, with no private provenance published.
- [ ] Three sanitized fixtures demonstrate where cross-run analysis adds value beyond incident diagnosis.
- [ ] One proposed improvement has reproducible verification.
- [ ] No expansion of Core execution authority or v0.1 scope.
- [ ] Public documentation and PR metadata contain no private source identifiers or operational details.
