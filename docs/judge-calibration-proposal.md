# Proposal: calibrating review and recovery judgments as one loop

Status: proposed; documentation only. This proposal belongs to the post-v0.1
[evaluation and independent adjudication milestone](post-v0.1-milestones.md#evaluation-and-independent-adjudication).
It does not add a v0.1 gate, change accepted policy, enable an automatic judge,
or claim that the experiment has run.

## Problem and intended outcome

A bounded review/fix loop can stop unnecessarily, continue making local repairs
when a design change is needed, or declare a concern resolved without enough
evidence. Its downstream halt auto-judge can recover a reasonable stop, but can
also send the same Work back into an equivalent failure. Optimizing either
judgment in isolation misses this interaction.

Evaluate review assessment, repair-strategy selection, and post-halt recovery
judgment as one episode. Calibrate each model prompt against evidence available
at its own decision boundary; judge the resulting system by safe, verified
progress and the human interventions required to obtain it.

The primary research question is: can a versioned set of judge prompts reduce
human interventions per eligible Work without increasing false completion,
unsupported finding clearance, or unsafe recovery?

## Scope and relationship to current design

"Convergence" here means the complete review/fix and halt/recovery loop, not
just the narrow publication classifier or a Worker-owned outer loop. "Halt"
is an analytical boundary at which routine correction stops or requires a
different decision; it is not a new lifecycle state or CLI command.

| Boundary | Proposed experiment target | Existing contract that remains authoritative |
| --- | --- | --- |
| Review assessment | Defect present/fixed/unknown; required here, advisory, defer or dismiss | Exact-target finding coverage and authorized disposition in [evidence gates](evidence-gates.md) |
| Repair strategy | Another bounded local fix, design-first repair, or human decision | Core-approved task, remaining budgets and [planning milestone](post-v0.1-milestones.md#planning-and-bounded-replanning) |
| Post-halt auto-judgment | Classify cause and recommend an eligible correction, recollection, repair wait or handoff | Failure classification, prerequisites and immutable RecoveryDecision in [evidence gates](evidence-gates.md#failure-classification-and-recovery-decisions) |

The intended integration home is taisan. Judges are replaceable, read-only
dependencies that propose evidence and actions. Core validates eligibility,
records authoritative decisions and authorizes effects; the execution loop
drives them. An LLM recommendation never replaces stop proof, exact target
checks, complete finding coverage, host-effect reconciliation or human authority.

Pre-publication invariant conformance is a separate assessment lane. It is
recorded as context when relevant to recovery, but its prompt is outside the
first calibration experiment. Do not combine a finding dismissal with a waiver
of a required invariant. See [adapter boundaries](adapter-boundaries.md),
[recovery policy](recovery-policy.md) and [execution loop](execution-loop.md).

Existing contracts specify bounded review and deterministic recovery rules.
A trainable post-halt judge, its adapter and prompt optimizer are proposed
extensions, not established runtime capabilities. Initial historical records
may come from predecessor workflows through an owner-controlled import. Keep
source-specific mappings and private identifiers outside publication-facing
docs; imported records are experimental evidence, never runtime state migration.

## Method and research basis

[AutoRefine: Auto-Calibrating Code Judges from Workflow Outcomes
(Shahidi et al., 2026)](https://arxiv.org/abs/2611.09291) motivates prompt
maintenance using workflow outcomes and a teacher that keeps labels only when
they can be explained from the judge's input. Its prompt optimizer analyzes
disagreements and selects revisions on separate validation data. Its production
study is small and reports different behavior on current and older workflows.

This proposal adopts outcome-aware filtering and prompt revision. Extending
them to paired review/recovery judgments and Work-level intervention metrics
is our hypothesis, not a result established by that paper.

## Episode and decision records

An episode begins at the first eligible review assessment and follows all
related halt/recovery decisions, resumed attempts and successor Executions
under the same Work. It ends at verified completion, an explicit terminal
operator decision, or the experiment's observation deadline. A human handoff
is an intervention event and can be followed by later decisions in that episode;
it does not automatically end observation. Unfinished episodes are censored,
not successes or failures inferred from silence.

Use one immutable decision record per actual decision attempt. Proposed export
fields are a design sketch, not a migration or a compiled Rust schema:

| Record | Required content |
| --- | --- |
| Identity | Experiment-local episode/decision IDs; scoped Store/Work/Execution/Invocation references; predecessor/successor links; boundary and timestamp |
| Subject | Specification, invariant-set and policy digests; exact base/head or artifact; review round and complete finding-set digest where applicable |
| Decision-time input | Canonical bytes/digest, available evidence references, missing/truncated/reclaimed bodies, checkout identity and permitted read surface |
| Judgment | Model/provider and effective model, prompt/schema/adapter versions, raw output, parsed recommendation, rationale and evidence references |
| Core result | Accepted/rejected/not-applied recommendation, reason, current guard facts, immutable decision ID, usage before/after and authorized action |
| Follow-through | New artifact/check/review evidence, material progress, equivalent failure recurrence, further halt, human actions and elapsed time/cost |
| Labels | Scope, author kind/protocol, origin, reason, evidence references, label version and decision-time observability |

Digest-only evidence whose body is unavailable cannot produce a replayable case.
Export reports must distinguish historical judgment evidence from complete
replay inputs. Record runtime/tool failures separately from a semantic verdict.
Never convert a missing result into an empty finding set.

Freeze input and outcome separately. An outcome-aware teacher may see the later
outcome, but the replayed judge sees only its original decision-time view.
Future commits, human corrections, final disposition and teacher explanations
must not be reachable through its checkout, filesystem, tools or network.

Experiment exports use an explicit retention and sanitization manifest.
Selection does not silently pin production bodies forever: copy authorized
bodies into the bounded experiment dataset before production reclamation, or
mark the case unavailable. Strip credentials and unnecessary personal data.
Never commit raw private traces to the repository.

## Labels and the abstaining teacher

Keep three distinct label layers:

1. Observed outcomes: check results, accepted artifacts, halt recurrence,
   manual interventions and terminal host states. These are facts, not a
   correctness verdict on every earlier decision.
2. Human adjudication: whether a decision was justified under the evidence,
   specification and authority available then. Declare the reviewer protocol,
   retain disagreements and allow unknown.
3. Teacher-curated weak labels: keep with an evidence-grounded explanation,
   abstain, or flag a possible label conflict for human review. They never
   become human approval or overwrite original labels.

For each boundary, the teacher receives exactly the original judge view plus
the candidate label and a bounded outcome description. Every retained rationale
must point to evidence in that original view. Outcome-only evidence may explain
why a label was proposed but cannot manufacture decision-time observability.
Reviewer comments can ground label adjudication; unless they were in the
original view, they cannot become evidence the replayed judge supposedly saw.

Teacher output includes `keep | abstain | conflict`, reason
(`input_insufficient | outcome_unrelated | label_conflict | supported`),
evidence references, and source label identity. Audit both retained and abstained
samples; abstention can hide genuine failures as well as remove noise.
Teacher agreement is not independent validation.

| Observation | Treatment |
| --- | --- |
| Final PR merged after several repair heads | Label only the exact decisions independently supported; merge does not validate earlier heads |
| Upstream halt followed by verified downstream recovery | Evaluate the pair; stopping was not necessarily an upstream mistake |
| Resume followed by the same equivalent failure without progress | Mark recurrence; adjudicate cause before assigning blame to a judge |
| Permission/config failure with unchanged prerequisites | Recovery recommendation must retain the repair hold |
| Missing result, incomplete trace or expired evidence body | Preserve unknown/unavailable; no correctness label |
| Another judge's rejection determined the outcome | Flag endogenous supervision; require independent evidence for evaluation |

Sample ordinary automatic completions and justified human handoffs alongside
overrides and failures. Collecting only interventions would bias the training
set toward continuing automatically.

## Calibration and replay design

Freeze a manifest containing dataset membership, grouped train/validation/test
assignments, allowed judge variants, label protocol, model settings, ceilings,
metrics, stopping rules and promotion criteria before optimization.

Group every head, decision, halt, retry and successor Execution of a Work into
one split. Related Works sharing a fix/incident or duplicated task belong to
one group. Freeze the split before teacher filtering. Curate training only;
validation and test retain the original sample and report unknowns, unavailable
inputs and abstentions without quietly removing difficult cases.

Retain current and legacy workflow slices separately, identified by model,
adapter, input contract, policy and prompt versions. Report regression on both;
do not pool a changed workflow into an apparently improved aggregate.

Run the following comparison matrix:

| Variant | Review/strategy judges | Halt judge | Purpose |
| --- | --- | --- | --- |
| Baseline | Frozen existing version | Frozen existing version, or explicit no-judge baseline | Reproduce current decisions |
| Upstream only | Candidate | Baseline | Attribute upstream effects |
| Recovery only | Baseline | Candidate | Attribute downstream effects |
| Paired | Candidate | Candidate | Check interaction and combined behavior |

Initially vary one upstream prompt at a time. Strategy machinery remains a
future extension if no comparable historical decision interface exists.

The optimizer sees training disagreements and teacher explanations, proposes
small balanced rules for accepting and rejecting actions, and retains evidence
locators for each edit. Prompt size is capped. Select on validation; lock the
candidate before one held-out test. Test failures require a new experiment
version, not repeated edits against the same test set.

Offline replay measures conditional decision quality on recorded inputs.
Changing an upstream decision changes which downstream states would be reached:
do not feed a candidate an incompatible historical continuation and claim
counterfactual intervention savings. Reject those continuations as unreachable
or unknown. Fixture continuations can test mechanics, but cannot establish
production effectiveness. End-to-end claims require the later prospective trial.

## Metrics and interpretation

| Level | Metric and denominator |
| --- | --- |
| Primary, prospective | Explicit decision/repair intervention events divided by all enrolled eligible Works; count zero-intervention Works and all successor Executions |
| Primary guardrails | Independently verified completion rate and false-completion/unsupported-clearance rates; pending/unavailable cases reported separately |
| Decision quality | Per-boundary confusion/abstention/invalid-output rates on adjudicated cases, false continue and false stop; agreement with uncertainty intervals |
| Loop behavior | Halts, repeated equivalent failures after resume, corrections and material-progress transitions per Work |
| Operational | Wall time, human-wait time, actual operator effort where measured, model cost, token usage and evaluation overhead |
| Coverage | Eligible/enrolled/excluded/censored Works, label and replay availability, teacher abstention by outcome and workflow slice |

A human intervention is one explicit decision or prerequisite-repair event
needed to progress the Work. Replayed commands, acknowledgments with no decision,
passive inspection and duplicate messages do not add events. Independent audit
effort is reported separately, not hidden or counted as runtime intervention.
Freeze the event taxonomy before collection.

Reducing interventions by abandoning Works, never escalating or incorrectly
clearing findings is not improvement. Report interventions alongside verified
completion, time-to-completion and unresolved work at the fixed horizon.
Historical interventions remain observed baseline facts; candidate intervention
counts are not estimable from decision replay alone.

Scott's pi may supplement binary per-boundary agreement; it is not the sole
objective for multiclass judgments or loop-level success. Show counts, class
balance and Work-grouped uncertainty intervals. A small feasibility sample
supports instrumentation decisions, not a production effectiveness claim.

## Safety and activation contract

The policy envelope is fixed during a calibration experiment. Candidate prompts
cannot change specifications, required invariants, human-only dispositions,
budgets, equivalence normalization, ownership or publication authority.

A recovery recommendation is accepted only after fresh Core validation of the
current Work version, target, finding coverage, repaired prerequisites, remaining
Work-wide usage, writer state and unresolved effects. Unknown writer state
requires process reconciliation; unknown host effects require reconciliation
of the original action. Neither becomes an ordinary retry or a success.

No proposal can clear Attention by acknowledgment, reset usage through a new
Execution, or silently override a human decision. Unsupported/malformed judge
output falls back to the existing deterministic hold/handoff behavior, never
implicit permission to continue.

Pin the active prompt bundle, model/adapter identity and experiment version in
effective policy for new Work. Record an explicit activation decision and rollback
reference. Existing Work retains its accepted policy unless the authorized
version-checked policy-change path updates it. Rollback affects future judgments
through that same policy mechanism; it cannot erase decisions, receipts or usage,
rewrite an issued Invocation, or replay an external effect.

The first version produces a report and a proposed prompt-bundle diff only.
It has no host mutation credentials and no authority to activate itself.

## Delivery plan and first experiment

All numbers below are proposed experiment defaults, configurable and frozen in
the manifest; they are not amendments to runtime recovery defaults.

| Stage | Concrete deliverable | Exit condition |
| --- | --- | --- |
| E0: inventory and contract | Evidence availability report, boundary map, export/label schemas and fixture episodes | Exact inputs and outcomes can be linked; missing interfaces/bodies are explicit |
| E1: capture and adjudicate | 30-50 historical Work episodes spanning ordinary completion, halt/recovery, recurrence and human handoff | Publish counts and label audit; insufficient rare classes yield an inconclusive result |
| E2: bounded calibration | At most 3 revisions per selected prompt; up to 2 replays per variant; baseline/ablation/paired report | Frozen held-out results, trace audit, cost accounting and candidate diff |
| E3: shadow operation | Candidate judgments on new cases alongside existing authority | No candidate effects; drift, coverage and proposed-action disagreements audited |
| E4: prospective evaluation | Separately approved guarded trial of one frozen bundle | Verified Work-level outcome/intervention evidence before any effectiveness claim |

E1/E2 use a proposed 60/20/20 grouped split with the seed and any necessary
stratification fixed before labels are inspected. Count Works, not the larger
number of decisions. Sparse test strata can make the result inconclusive.
The experiment requires a configured model-call/token/cost ceiling and deadline;
absent ceilings refuse to start. Exhaustion preserves partial evidence and yields
an incomplete report. Do not guess a universal monetary default before E0.

E0-E2 acceptance requires:

- A reproducible manifest, exact-input digests and a replay availability report.
- Separate per-boundary and paired results; no counterfactual savings claim.
- Human audit of every candidate unsafe-continue/unsupported-clearance discrepancy
  plus a prespecified sample of kept and abstained labels.
- No regression on mandatory safety fixtures, including stale/same-head finding
  growth, changed prerequisites, budget exhaustion, unknown writer/effect,
  malformed output and retained operator decisions.
- An explicit conclusion: improve inputs, collect more evidence, revise a prompt,
  continue shadowing, or reject the hypothesis.

Before E4, freeze numeric efficacy/non-inferiority margins, required sample size,
observation horizon, assignment unit and stop criteria based on E0-E3 evidence.
Use Work-level assignment and audit shared-resource interference. A historical
30-50-episode pilot cannot justify the activation gate. An observed safety
violation stops the candidate trial and triggers rollback/handoff.
Unset efficacy margins mean "no promotion decision", not permission to deploy.

## Proposed implementation boundaries

Keep the experiment toolkit separate from the runtime decision path, initially
as an offline taisan tool. Proposed operations are export, adjudicate, curate,
replay, refine and compare; their exact CLI/API shape is deferred to E0.
They are not newly available `taisan` commands.

The runtime supplies immutable evidence and, later, an explicitly configured
read-only judge port. The toolkit produces versioned datasets, bundles and
reports. Core retains all transition and effect authority. Integrate with the
existing evaluation milestone rather than adding a second adjudication ledger.

Implementation follow-ups should be executable slices: E0 export/schema and
fixtures; E1 adjudication/import; E2 replay/optimizer/report; then E3 shadow
adapter. Each needs contract checks for identity, leakage, missing evidence,
budget exhaustion and report denominators. Do not begin by replacing the whole
recovery controller with a learning agent.

## Decisions still to resolve

- Which decision-time inputs are actually retained for both boundaries, and
  which need new capture before a usable paired dataset exists?
- Is the first upstream target finding treatment, fix verification, or repair
  strategy? Choose from label availability at E0; keep halt judgment in scope.
- Which human labeling protocol is feasible, and which audit subset needs a
  second independent reviewer?
- How should a genuinely new input view be compared? Treat it as a separate
  experiment, not an unrecorded prompt improvement.
- What Work-level prospective sample and safety margins support activation
  after the feasibility experiment?

These questions block the relevant implementation or promotion stage, not
review of this proposal. No result or production readiness is asserted here.
