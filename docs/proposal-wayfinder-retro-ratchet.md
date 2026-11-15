# Proposal: Decision Mapping, Evidence-Driven Retro, and Quality Ratchets

Status: **Proposed for discussion — no implementation authorized**  
Date: 2026-11-15  
Scope: taisan's optional planning and improvement capabilities, not a new execution authority.

## Summary

Adapt three ideas from Matt Pocock's public Software Factory materials, **without copying their issue-driven orchestration into taisan Core**:

1. **Wayfinder → decision mapping before Work acceptance.** Track unresolved decisions and investigations separately from executable Work. Promote a resolved plan into candidate Work only with explicit human approval.
2. **Retro → evidence-driven factory improvement.** Analyze completed, failed, and human-interrupted executions; propose targeted improvements with citations to durable evidence and evaluate whether they actually reduce intervention.
3. **Ratchet → monotonic, versioned quality constraints.** Prevent newly introduced violations while gradually retiring a baseline of existing violations, without treating an exception as a passing invariant.

The strongest near-term candidate is a **read-only Retro prototype** over existing event/observation records. Wayfinder is useful but should remain an optional selector-side workflow. Ratchet should initially live in repository checks and approved Work verification profiles, not as a new Core stage.

## Source material and interpretation

Primary public sources (checked 2026-11-15):

- [Matt Pocock's public skills](https://github.com/mattpocock/skills), especially [Wayfinder skill](https://github.com/mattpocock/skills/blob/main/skills/engineering/wayfinder/SKILL.md), [To Tickets](https://github.com/mattpocock/skills/blob/main/skills/engineering/to-tickets/SKILL.md), and [Architecture Improvement](https://github.com/mattpocock/skills/blob/main/skills/engineering/improve-codebase-architecture/SKILL.md).
- [Software Factory example tasks](https://github.com/mattpocock/skills/blob/main/skills/in-progress/software-factory/EXAMPLE-TASKS.md): concrete examples for Retro, Ratchet, failed-run repair, architecture scans, and chained issue workflows.
- [Course exercise repository](https://github.com/ai-hero-dev/cohort-005-project), an **exercise**, not proof of production robustness.

**Observed vs inferred:** Wayfinder's map/decision tickets/frontier are explicitly specified in its skill. Retro and Ratchet examples explicitly define triggers and outputs, but the full reliability and measured impact of a production deployment are not established by the public exercise. The proposed taisan integrations below are **our designs**, not features claimed to exist in Matt's implementation.

## Current taisan baseline: design and implementation

Read and cross-checked against the repository on 2026-11-15:

| Area | Current contract / code | Implication |
| --- | --- | --- |
| Human authority | `docs/principles.md`, `README.md`: humans choose objectives; selection proposes only | Wayfinder cannot silently accept or reprioritize Work |
| Selector | `docs/adapter-boundaries.md`: candidates, source freshness, allowlist, no authority to dispatch | Decision mapping belongs upstream of `submit` |
| Durable identities | `docs/glossary.md`, `docs/lifecycle.md`: Work / Execution / Invocation, immutable approved spec | Retro must correlate across retries without conflating attempts |
| Evidence and stage truth | `crates/workloop-core/src/stage.rs`: stage order, typed status, revision-bound evidence | Retro should consume recorded facts, not worker narratives alone |
| Command authority | `crates/workloop-core/src/work.rs`: version-aware idempotent command decisions | Improvement suggestions must not directly mutate Core |
| Quality | `docs/evidence-gates.md`: mechanical checks, optional invariant conformance, optional independent review, configured CI | Ratchet is not a substitute for any existing gate |
| Recovery | `docs/recovery-policy.md`: bounded fixes, equivalent-failure detection, progress, human handoff | Retro should target these failure modes and compare pre/post outcomes |
| Release scope | `docs/v0.1.md`: first real Work through PR/review/verified outcome | None of these three should block v0.1 |

**Implementation caveat:** The README says the Rust workspace is currently a fixture-backed slice-0 implementation; real host/worker/selector integrations are not yet delivered. The linked docs often describe **target contracts**, not running capabilities. This proposal must not be read as claiming that full event extraction, evaluation, or automated improvement already works.

## A. Wayfinder: decisions before execution

### What is valuable in the source

Wayfinder defines a destination, a parent map issue, child **decision tickets** (research, prototype, human discussion, prerequisite task), native blocking edges, and a frontier of unblocked, unclaimed decisions. It records uncertainty as **fog of war** rather than fabricating a complete task graph. A closed decision has a recorded resolution; implementation tickets are a separate handoff. This is distinct from splitting a known spec into vertical implementation slices.

### Fit with taisan

Taisan's Work is a *meaningful accepted implementation objective*, not an issue or planning question. Therefore **do not model Wayfinder decision tickets as Work**, and do not make the Core queue into a generic planning board.

Suggested optional upstream pipeline:

```text
Human goal
  → DecisionMap (external issue tracker / Markdown, optional)
  → Research / Prototype / Human decisions
  → Resolved plan + decision provenance
  → Selector Candidate(s) with explicit constraints and verification
  → Human approval → existing submit → Work/Execution
```

A lightweight selector adapter or CLI-backed skill may implement the map. Store links and immutable summaries of accepted decisions in the approved specification; do not synchronize a second decision state machine into the Work store.

### Important contracts

- A decision map is **not** a Work, Execution, or Invocation and consumes no execution capacity.
- The agent cannot answer a human-only decision on the human's behalf.
- A decision resolution is linked to its source and revision; a reopened decision invalidates derived proposals until reviewed.
- Map frontier is a planning convenience, **not** dispatch eligibility.
- Avoid adding native dependencies to the v0.1 built-in queue, whose non-goals explicitly include generic card dependencies.
- If multiple issues form one coherent Work, preserve taisan's grouping policy rather than adopting one ticket = one PR.

### First experiment

Take one real, still-ambiguous feature request. Create a map with 3–6 decision tickets and at least one genuine unresolved question. Compare (a) resulting approved Work/spec completeness, (b) number of human clarifications during execution, and (c) unnecessary decomposition. Stop if maps become ceremony for already-clear requests.

## B. Retro: learn from failed or expensive runs

### What is valuable in the source

The public example triggers a Retro on a sampled merged PR (one in ten), reads the ticket, automated reviews and run logs, and proposes instruction changes as a reviewed PR. Separate examples file failed-run repair tickets and nightly architecture scans. These are **suggestion-producing workflows**, not demonstrated autonomous proof of improvement.

### Taisan adaptation

Prefer **event-triggered + stratified sampling** over only every tenth merge. Merges miss precisely the failed, halted and human-interrupted cases that matter most to taisan.

```text
Terminal Work / Attention resolution / repeated equivalent failure
  → immutable evidence bundle
  → failure & intervention classification
  → candidate improvement (policy / prompt / test / adapter / architecture)
  → human-reviewed PR or issue
  → controlled replay / regression test / Trace2Eval where possible
  → compare future intervention and cost
```

A read-only `RetroAnalyzer` can initially be a separate CLI utility, consuming exports of persisted Work, Execution, Invocation, Observations, stage records, command decisions and relevant PR snapshots. It should not write state or invoke a Worker through private paths.

### Proposed evidence bundle

- Stable `work_id`, execution and invocation IDs; original approved spec and policy digest.
- Trigger category, timestamp, stage, outcome and current/previous Attention.
- Produced artifacts and published head(s); checks and invariant/review results **bound to the correct revision**.
- Recovery attempts, equivalent-failure signatures, elapsed wall time, worker time, token/provider cost **only when measured** (otherwise unknown).
- Human decisions: what was requested, why, and whether execution could resume.
- Source pointers and provenance for every generated diagnosis.

### Candidate schema (illustrative; not a new Core API)

```yaml
retro_id: retro-...
trigger: failed_work | attention_resolved | repeated_failure | sampled_success
work_id: ...
evidence_refs: [...]
failure_signature: ...
hypothesis: ...
intervention_type: prompt | policy | check | code | docs | adapter
proposed_change: ...
expected_effect: fewer_human_interventions | fewer_retries | lower_cost | shorter_latency
verification_plan: ...
confidence: low | medium | high
status: proposed
```

### Evaluation and guardrails

Primary metric: **human interventions per comparable accepted Work**, not total merged PRs or raw agent activity. Secondary: successful outcomes, equivalent-failure recurrence, review rounds, time, and cost. Stratify by Work complexity and type; otherwise selecting only easy tasks can falsely improve the metric.

- Separate a diagnosis from a fix: no auto-edit to instructions or policy merely because an LLM suggested it.
- Factory-self-modifying changes require human review and normal CI.
- Never relax evidence, approval, ownership or writer-safety invariants as a way to improve throughput.
- Replays must use a pinned initial revision, original task and explicit evaluation criteria. Trace2Eval can supply comparable tasks, **not automatically causal attribution**.
- Preserve unknown evidence; do not classify missing telemetry as success.
- Deduplicate repeated incidents by a stable signature, with an escape hatch for new evidence.
- Do not recursively spawn repair of a failed repair.

### First experiment (recommended first)

Export 10–20 historical cases from the source workflow or fixture traces, including clean success, repeated review failure, publication mismatch, stale review and human intervention. Generate a read-only report with evidence-linked failure clusters and no more than 3 ranked proposals. Human-check precision and novelty. Compare against known fixes; only then propose automated triggers.

## C. Ratchet: make quality improvement monotonic

### What is valuable in the source

The public example enables a stricter lint/type check for passing files, maintains an **exception list** for legacy failures, and uses a daily task to fix one exception and open a PR. The crucial property is not the schedule: **new violations are blocked while existing debt shrinks**.

### Fit with taisan

Taisan already separates mechanical verification, immutable invariant sets, optional LLM conformance and revision-specific review. Ratchet should operate **outside** that semantic model as a versioned check configuration / exception baseline, then feed its check result through the existing mechanical or named-CI gate.

Proposed data for each check:

```yaml
rule_id: architecture.no_cross_layer_import
rule_version: 1
scope: repository
baseline_revision: <sha>
exceptions:
  - stable_fingerprint: <rule + path + diagnostic identity>
    reason: legacy violation
    owner: ...
    expiry: optional
```

Comparison must be against a reproducible baseline; simple line numbers are unstable. Reject any *new* violation not in the approved exception set. A removal of an exception requires the check to pass and evidence to show the debt was eliminated. New exceptions require explicit approval and recorded rationale; they are not created automatically by the repair worker.

### Important distinction from Invariants

- **Invariant:** a required property of this Work's approved intent, checked against a specific produced artifact.
- **Ratchet:** a repository-wide (or scoped) quality rule that tolerates an explicit legacy baseline while preventing regression.
- An exception is **not** evidence of invariant conformance. Never allow a baseline to waive a Work-specific approved invariant.
- The rule version and exception set must be pinned to the relevant verification result; a changed rule invalidates stale check evidence.
- CI must remain green after each migration step; prefer small PRs and an approved expand–contract plan when broad migrations cannot land independently.

### First experiment

Choose one deterministic architectural rule (for example, forbidden crate dependency direction or a prohibited direct call across an adapter seam). Record existing violations; make CI reject any new violation; fix one exception through an ordinary reviewed PR. Measure false positives, exception churn and review burden before adding scheduled remediation.

## Architectural options and recommendation

| Option | Wayfinder | Retro | Ratchet | Assessment |
| --- | --- | --- | --- | --- |
| A. Core-native entities and stages | New map entities | New stage | New stage | **Reject for now:** expands Core and release scope, duplicates external tracker/checks |
| B. Optional adapters / skills + existing evidence gates | Selector-side planning | Read-only analyzer and proposed PRs | Repository CI and configured check | **Preferred:** preserves authority and existing contracts |
| C. Independent autonomous improvement agent | Autonomous planner | Auto-modifies factory | Auto-remediates and merges | **Defer:** high authority and regression risk; no demonstrated effectiveness |

### Proposed order and exit gates

**P0 — Evidence audit (documentation only).** Confirm which persisted fields and export interfaces exist in the current Rust slice versus only target docs. Trace the data path from `work.rs`, `stage.rs`, `observation.rs`, store implementation, and CLI query. Record missing inputs before claiming an analyzer can run.

**P1 — Read-only Retro spike.** Offline fixture/historical traces → evidence bundle → clustered report → human-reviewed suggestions. Exit when diagnoses have reproducible references and measured precision.

**P2 — Ratchet pilot.** One deterministic check and approved exception baseline; new regressions fail CI; one legacy violation removed by PR. Exit when no false pass and the exception count does not silently grow.

**P3 — Wayfinder pilot.** Optional issue-tracker map → approved Candidate/Work handoff for one ambiguous objective. Exit when the resulting specification has clear constraints, verification and decision provenance without bypassing human intent.

**P4 — Controlled improvement loop.** Only after P1–P3, connect retros to candidate changes and replay evaluations. Require explicit approval for policy/instruction modifications, and independent proof before adopting a new default.

These are experiments, **not v0.1 release gates**.

## Open decisions for owner review

1. Should Retro consume source-system traces until real taisan integrations exist, or should the first pilot use only the fixture-backed slice-0 store?
2. Which human interventions count for the KPI (clarification, review override, recovery, merge approval), and how are mandatory approvals separated from avoidable intervention?
3. Where should Wayfinder maps live by default: GitHub child issues or Markdown documents? No new Core store is proposed.
4. Which first Ratchet rule is genuinely useful in taisan, with a low-noise deterministic checker?
5. Should proposed improvement PRs be allowed to modify policy defaults, or only prompts/docs/tests in the initial experiment?

## Non-goals

- Replacing existing Work / Execution / Invocation identities.
- Treating planning tickets as executable Work or adding generic issue dependency scheduling to Core.
- Letting Retro automatically approve its own modifications.
- Weakening invariant conformance or treating a legacy baseline as a pass.
- Auto-merge or deployment authority.
- Requiring these capabilities for v0.1.

## Acceptance criteria for this proposal

- [x] Distinguish confirmed public-source behavior from inferred implementation.
- [x] Compare all three ideas against taisan's current documented contracts and representative Rust modules.
- [x] State target-design versus implemented-slice limitations.
- [x] Define integration boundaries, failure/safety constraints, initial experiments and measurable gates.
- [ ] Owner reviews the open decisions.
- [ ] Full source audit validates export availability and exact implementation seams before an implementation issue is accepted.
