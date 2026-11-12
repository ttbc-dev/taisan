# Post-v0.1 milestone scope

The owner approved deferring the following seven behaviors from v0.1 and retaining them for later milestones. These are committed roadmap scopes, not optional omissions. The scopes are registered as GitHub milestones linked below. This document does not authorize implementation or create implementation issues.

## Planning and bounded replanning

Track delivery in [milestone 3](https://github.com/shimpeiws/taisan/milestone/3).

| Behavior | Scope and acceptance |
| --- | --- |
| `worker-B004` | Specify planner failure classes and bounded replanning. Distinguish malformed output, provider/execution failure, and a falsified plan premise. Replanning and execution retries consume their own durable bounds without resetting Work recovery limits. Cover both worker planning and converge repair planning |
| `worker-B008` | Add an optional worker planning session before implementation, retain its plan evidence, and validate missing, malformed, and valid outputs. Define explicit entry and failure behavior |
| `TGT-B07` | Enforce a read-only tool surface for the worker planning stage. Prove write attempts are refused and repository-host mutation credentials are unavailable. Preserve request identity and outer time bounds |

Converge repair planning is distinct from worker-internal planning: it classifies findings and binds a correction plan to the current head, findings, and human decisions. Track its detailed behavior through `worker-B042` as well as `worker-B004`; deferring planner machinery does not remove the v0.1 bounded review/fix loop, finding coverage, approval, or recovery requirements. Do not copy worker-owned outer loops or host publication into the managed Worker entry point.

The owner also approved two follow-up capabilities under this milestone. Automatic PR-conflict repair must bind its plan and evidence to both source and target revisions and preserve publication, approval, and recovery guards. Dedicated external completed-fix adoption must bind a retained task, exact artifact and manual actor, retain an immutable evidence snapshot, and run fresh independent verification and review. v0.1 detects conflicts for human handling and supports claimed manual workspace repair through ordinary resume; neither capability is a v0.1 delivery gate.

## Evaluation and independent adjudication

Track delivery in [milestone 4](https://github.com/shimpeiws/taisan/milestone/4).

| Behavior | Scope and acceptance |
| --- | --- |
| `worker-B061` | Human labeling and adjudication ledger bound to the dataset. Enforce declared reviewer protocol and distinguish human from model labels |
| `worker-B065` | Archive inventory and same-head reviewer comparison. Distinguish absent, invalid, missing-identity, and incompletely fetched evidence; incomplete reads cannot establish a complete denominator |
| `worker-B067` | Deterministic per-head comparison trials with explicit exit criteria. Unset thresholds remain undecided; heuristic finding pairing is not correctness proof |
| `worker-B129` | Blinded evaluator briefs bound to one case and actual evidence anchors. Check identity and automatable answer leakage, with a human pass for paraphrased leakage |

The evaluation machinery is deferred, not its general safety principles. v0.1 must not misrepresent model judgments as human approval, count incomplete evidence as complete, or default missing acceptance criteria to success. #51 carries these principles into the evidence contract. Independent evaluation claims must remain unsupported until the corresponding later milestone is verified.

## Proposed judge calibration experiment

The [judge calibration proposal](judge-calibration-proposal.md) describes an
E0-E4 investigation of review/fix judgments together with post-halt recovery
judgments. It proposes decision-time evidence capture, independent labels,
blinded prompt replay and a later Work-level trial measuring interventions
alongside verified completion. It is a proposal under milestone 4, not an
additional committed milestone, implementation authorization or v0.1 gate.
