# Evidence, invariant checks, and review gates

Status: concrete design for #51, based on the merged [identity/storage design](identity-storage.md) and owner-approved [recovery policy](recovery-policy.md) in PR #57. Documentation only: no runtime schema/API, configuration, credentials, checks, publication or E2E behavior is implemented here. This document records the owner's success-policy choices and supersedes earlier unconditional review/aggregate-check assumptions for the general v0.1 profile. The representative S1 demonstration still enables both assessment lanes to exercise them.

## Owner-approved lanes and caller flow

| Lane | Requirement and recorded result |
| --- | --- |
| Mechanical verification | Run the Work's declared test/build/lint commands against the exact produced change. Passing evidence is required for each declared command |
| Invariant conformance | Separate configurable on/off, default on. Compare implementation and evidence with the exact approved invariant set before publication. On violations, bounded fix/recheck; unresolved or unavailable result becomes Attention. Off means not performed, never proven conformant |
| Code review | Independently configurable on/off. On means separately requested review, bounded fix/re-review and Attention/human handoff for remaining findings. Off is explicit, not a fabricated clean review |
| GitHub CI | Optional configured additional named checks. If none are configured, do not wait for GitHub CI. A configured check must pass on the target change/head; missing/unavailable is not success |
| GitHub approval / merge rules | No Approve requirement or success-policy knob. Do not import branch-protection/ruleset requirements or change them. Automatic review creates no native approval/request-changes or draft-based merge gate |

Core owns the predicates and the requirement to obtain fresh observations. Host/Worker adapters perform reads and bounded tasks; the CLI loop drives those requests. A status Query never refreshes a remote source. A generic worker exit or model summary is not an Outcome.

1. Accept and approve Work/specification/invariant-set identity and a lane-policy snapshot. Config is resolved by the setup/adapter boundary, not parsed ad hoc in Core. New requirements cannot silently weaken accepted Work or reset Execution or Work-wide usage. The approved [recovery policy](recovery-policy.md) governs defaults and authorized policy updates; the [installation configuration](installation-config.md) specifies validation and the [execution loop](execution-loop.md#command-surface) specifies Commands.
2. Core requires the authoritative pre-dispatch source read from #50. After matching approved input, dispatch one bounded coding Invocation. Keep request, artifact, verification and process identities distinct.
3. Finalize an immutable change after writer-stop proof. Verify retained git objects, exact produced commit and declared mechanical command results. When invariant conformance is on, dispatch the configured invariant-check task for that exact change/specification. A valid negative assessment drives bounded repair; it does not become success or an invalid record.
4. Only after the enabled pre-publication gates pass does Core authorize separate guarded push and change-request create/update intents. The host adapter alone publishes. Keep each successful effect even if a later one fails or becomes unknown.
5. A normal new PR is published ready for human review after pre-publication verification, independently of optional code-review completion. If code review is on, run its separate task on the immutable current diff. Findings can drive bounded fixes; each changed artifact repeats mechanical and enabled invariant checks before republishing, then receives new code review. Do not revert the PR to draft or emit native Request changes/Approve as automatic review policy.
6. Observe only the additional configured CI checks. They may run concurrently after publication; await them before Work success, not by inventing GitHub merge restrictions. Review-off or CI-empty records identify what was skipped.
7. Core records a typed Outcome proof after all enabled requirements and required effects are satisfied, and all writers/effects are safe. Otherwise retain bounded repair or current Attention. Human merge remains independent and outside Work success; no merge is performed by this loop.

A configured review provider/task is the expected source, not every bot that comments on a PR. A separately executed review can use the same provider/model as coding; a coding report cannot certify that the review task ran. Host-native feedback, if published, uses ordinary non-blocking feedback/comments with durable intents. Existing repository rules and third-party reviewers remain external facts; taisan neither installs nor bypasses those rules.

## Evidence schema and validity

Target Rust shapes reuse #50's scoped IDs, immutable payload references and canonical-byte versioning. Constructors validate input once at Worker/host/persistence boundaries. Store structural FKs and transaction checks prevent cross-subject substitution; Core evaluates significance and policy. These are type sketches, not compiled APIs.

```rust
struct AssessmentSubject {
    work: WorkRef,
    execution: ExecutionId,
    spec: SpecRevision,
    invariants: InvariantSetRef,
    policy: PolicySnapshotRef,
}
struct DiffTarget {
    change: ChangeRequestRef,
    head: CommitRevision,
    comparison_base: CommitRevision,
    target_revision: CommitRevision,
    diff_digest: Digest,
}
struct EvidenceEnvelope<T> {
    id: EvidenceId,
    format_version: EvidenceFormatVersion,
    subject: AssessmentSubject,
    producer: ProducerRef,
    provenance: Provenance,
    payload_ref: RetainedContentRef,
    payload_digest: Digest,
    value: T,
}
enum Provenance {
    Invocation { request: RequestId, invocation: InvocationId },
    HostObservation { observation: ObservationId, source: AdapterSourceId },
    Operator { command: CommandId, actor: CommandActor },
}
struct PreparedChange {
    artifact: ArtifactId,
    invocation: InvocationId,
    repository: RepositoryRef,
    base: CommitRevision,
    produced: CommitRevision,
    git_objects: RetainedContentRef,
    content_digest: Digest,
}
struct VerificationResult {
    requirement: VerificationRequirementId,
    artifact: ArtifactId,
    commit: CommitRevision,
    argv: Vec<String>,
    environment: VerificationEnvironmentRef,
    result: CommandResult,
    output: RetainedContentRef,
}
enum CommandResult { Passed, Failed { exit_code: i32 }, Inconclusive { reason: Reason } }
struct InvariantSet {
    specification: SpecRevision,
    requirements: NonEmptyVec<InvariantRequirement>,
    canonical_digest: Digest,
}
struct InvariantRequirement {
    id: InvariantId,
    statement: RetainedContentRef,
    mechanical_requirements: Vec<VerificationRequirementId>,
}
struct InvariantAssessment {
    artifact: ArtifactId,
    commit: CommitRevision,
    covered_set: InvariantSetRef,
    items: Vec<InvariantAssessmentItem>,
}
struct InvariantAssessmentItem {
    invariant: InvariantId,
    conclusion: InvariantConclusion,
    supporting_evidence: Vec<EvidenceId>,
    rationale: RetainedContentRef,
}
enum InvariantConclusion { Conforms, Violated, Inconclusive }
enum LaneProof<T> { DisabledByPolicy { policy: PolicySnapshotRef }, Enabled(T) }
```

An approved invariant set contains named requirements, not an implicit empty success. Disabling the checker does not create or waive the specification itself.

`InvariantSetRef` resolves to immutable Store/Work/specification-scoped metadata and a canonical set digest. Approval binds exact specification, invariant set and lane policy, not an arbitrary latest set. Reject duplicate/unknown invariant IDs. The invariant-check result must cover every required ID exactly once; omitted/inconclusive items cannot pass an enabled lane. A positive item requires retained supporting references to the exact artifact/source/check records and rationale; missing references or required bodies make that item incomplete rather than positive. Core validates source, schema, target, coverage and references; it does not claim mathematical proof of free-text invariants. The configured checker is a trust boundary with retained reasoning/evidence, not an unverified string `all good`. Off skips the conformance task, not the declared mechanical checks or specification approval.

Validity and requirement outcome are separate dimensions:

| Record | Validity | Requirement outcome / action |
| --- | --- | --- |
| Exact completed test exits nonzero | Valid negative evidence | Mechanical requirement failed; repair/Attention, not publication |
| Exact invariant assessment reports violation | Valid negative evidence | Enabled conformance requirement failed; bounded fix/recheck |
| Exact complete review contains findings | Valid evidence | Bounded review/fix policy, possibly human handoff |
| Exact named CI result is failed/pending | Valid evidence | Failed or still waiting; cannot satisfy that configured check |
| Wrong Invocation/repository/specification/head, malformed envelope or digest mismatch | Invalid for this assessment; preserve original and reason | No publication/success authority; do not silently retarget |
| Missing body/result/source or interrupted pagination | Absent/unavailable/incomplete, explicitly identified | Not an empty result or a passing requirement; recover within bounds or Attention |
| Complete enabled review has zero findings | Valid complete-empty snapshot | Review requirement may pass for that exact target |
| Review/invariant check disabled | Valid policy fact, no assessment evidence | Record not performed; never present as a passed assessment |

The artifact must retain reconstructable git objects, not just a SHA or an ephemeral path. A successful command for a different artifact or argv does not cover this one. Retain exact source/verification text or durable content references and digests needed to reconstruct what was checked. #52 specifies transport/handoff and #54 retention/pins. A referenced body that is unavailable cannot be replaced by a digest-only assertion when its contents are necessary to evaluate a gate.

## Immutable review snapshots and finding identity

Use immutable round/source snapshots and complete assessments, with append-only finding occurrence/disposition history. Avoid mutable global finding status or text-similarity merging.

```rust
struct ReviewRound {
    id: ReviewRoundId,
    subject: AssessmentSubject,
    target: DiffTarget,
    source: ReviewTaskSourceRef,
    snapshot: ReviewSnapshotRef,
    evidence_generation: EvidenceGeneration,
}
enum SourceResult<T> {
    Complete(T),
    Partial { reason: Reason },
    Unavailable { reason: Reason },
}
struct ReviewSnapshot {
    source_revision: SourceRevision,
    provenance: Provenance,
    occurrences: Vec<FindingOccurrence>,
    canonical_set_digest: Digest,
}
struct FindingOccurrence {
    id: OccurrenceId,
    source_key: SourceFindingKey,
    content: RetainedContentRef,
    severity: ReportedSeverity,
    position: Option<DiffPosition>,
    thread: Option<HostThreadRef>,
}
struct Judgment {
    id: JudgmentId,
    round: ReviewRoundId,
    set_digest: Digest,
    decisions: Vec<OccurrenceDecision>,
    actor: CommandActor,
}
enum Disposition {
    Fix,
    Dismiss { rationale: RetainedContentRef },
    Accept { rationale: RetainedContentRef },
    Wait,
}
struct OccurrenceDecision { occurrence: OccurrenceId, disposition: Disposition }
struct FindingRelation {
    predecessor: OccurrenceRef,
    successor: OccurrenceRef,
    reason: RetainedContentRef,
    evidence: Vec<EvidenceId>,
}
```

A source key is scoped to its configured provider and request/report or host/repository/change/native identity. A local occurrence belongs to one immutable round/snapshot. An edited comment or new report revision yields a new occurrence version. Equal prose/path/line or equal provider-local numbers do not prove identity. Cross-attempt relations explain history but never copy a dismissal/acceptance or claim a fix on a new head. Absence from a later *complete* review can help satisfy the current review requirement; it does not alone prove a specific earlier finding was repaired. Keep the actual fix/artifact/publication/reassessment chain for any `fixed` disposition claim.

The initial reference workflow uses the one separately requested configured review task/result. An adapter may normalize a configured external source under the same complete-snapshot contract; arbitrary unsolicited bots do not become required sources. Preserve unsolicited observations with provenance, without silently adding mandatory findings or waiting forever for those bots.

A complete snapshot means the requested immutable report or all required source pages were obtained without omissions/errors under the recorded target. An empty partial response cannot seal a clean round. Host reads are not remote atomic locks: record head/base/target revision before and after collection and return partial/stale if a change is detected. The last authoritative source read is the observational boundary, with the same unavoidable later-change limit agreed in #50. New locally accepted evidence invalidates dependent authorizations atomically; no unseen-remote-change guarantee is claimed.

`Fix`/`Wait` remain unresolved. An operator's explicit Dismiss/Accept judgment requires current exact coverage and retained rationale; neither means the code was fixed or a host thread resolved. Review on runs bounded fix/re-review for actionable findings and routes remaining unresolved findings to Attention. It does not automatically accept findings when budget expires or silently invent permission for higher-risk operations. Human disposition authority and CLI representation follow the configured operator identity from the [installation configuration](installation-config.md) and the [execution loop](execution-loop.md); arbitrary reviewer text cannot grant it.

### Same-head growth and exact-set guards

Queries return WorkVersion, specification/invariant/policy identity, current round ID, exact diff target, complete-set digest and source availability. Judgment acceptance, release, next-stage dispatch and dependent effect authorization each recheck these against saved current state in one transaction. Decision keys must equal the current snapshot occurrence set exactly: no missing, duplicate, unknown or extra ID. Complete-empty reviews can receive an internal empty assessment without forcing a meaningless empty human judgment Command.

Only material gate changes advance this generation: exact target/specification/policy changes, requested source snapshot/availability changes, named required check attempt/conclusion changes, or required evidence-body validity changes. Identical polls, duplicate events and unrelated unsolicited bot observations do not invalidate an assessment.

If finding B arrives on the same head after inspecting `{A}`, a new immutable snapshot/round is installed. Increment evidence generation and WorkVersion, invalidate dependent judgment/release pointers, and reject the old set at acceptance or release. Duplicate identical delivery changes neither. Source unavailability invalidates completeness without deleting the old snapshot. Out-of-order notifications trigger a fresh read and cannot restore a superseded current pointer. Diff-base/target movement invalidates location/review even if source head is unchanged.

The same vector guards already-authorized fix/reply/thread actions. A remote change after the final read does not undo an applied operation; retain its original receipt and invalidate subsequent uncommitted steps. Existing Invocation requests remain immutable. This closes the local same-head finding race in the target design, not in current #24 runtime code; the defect remains open until implemented and tested.

## Stage effects and reconciliation

Before the first write for a stage, Core saves its exact required effect set, target, actor, payload digests and prerequisites. Derive it from the accepted task/judgment and supported workflow, not whichever receipts a Worker returns. Empty/inapplicable effects have a recorded reason. Every effect has a separate ActionId; multi-call host operations must preserve constituent identities/partial results under the host contract.

| Step | Required evidence before authorization | Result required for this step |
| --- | --- | --- |
| Finalize change | Approved spec/invariants/policy, producing Invocation, writer-stop proof | Retained exact artifact/git objects and declared mechanical verification |
| Invariant conformance, when on | Exact artifact/commit and complete approved invariant set | Valid complete positive assessment; negative/inconclusive cannot authorize publication |
| Push | Passing pre-publication lanes, saved branch and absent/exact prior ref, actor/capability | Typed receipt for the exact produced commit and guarded non-force ref update |
| Open/update change request | Independently verified push and exact repository/branch | Separate create/update receipt; failed create never erases successful push |
| Code review, when on | Exact current diff target, requested review task/source | Complete current review snapshot, bounded fix/review or explicit human handoff |
| Fix publication | Current exact-set Fix decisions, remaining Invocation budget, immutable starting revision | New artifact/mechanical checks, enabled invariant reassessment, guarded push and fresh review target |
| Required reply/thread operation | Exact current occurrence/decision/thread target and published fix evidence when claiming a fix | One marked comment/reply or resolution receipt per requested effect; a reply is not thread resolution |
| Ready state, when a task explicitly requires it | Verified publication and the applicable task prerequisites | Fresh already-ready observation or separately receipted ready action; never used to hold a PR draft for optional review |
| Settle | All enabled lanes passed or authorized review dispositions settled; required CI/effects current; writers/effects safe | Frozen typed Outcome proof and one terminal release transaction |

The reference automatic workflow does not create native Approve or Request changes intents. Those remain optional operations in the general host port, not this Work-success profile or merge policy. Do not post unresolved inline threads or change PR draft state solely to enforce automated review as a merge barrier. Existing external threads may require explicit replies/actions as part of a supported task, with their own authority and receipts.

```rust
struct RequiredStageEffects {
    subject: AssessmentSubject,
    purpose: StagePurpose,
    actions: Vec<RequiredActionRef>,
    inapplicable: Vec<EffectNotRequiredReason>,
}
struct EffectReceipt {
    action: ActionId,
    intent_digest: Digest,
    subject: AssessmentSubject,
    intended_actor: HostActorRef,
    target: HostTarget,
    provenance: ObservationId,
    result: EffectResult,
}
enum EffectResult {
    Applied { authenticated_actor: HostActorRef, fact: NormalizedEffectFact },
    Failed { no_effect: NoEffectProof, reason: Reason },
    Unknown { reason: Reason, partial_facts: Vec<NormalizedEffectFact> },
}
enum NormalizedEffectFact {
    RefPublished { repository: RepositoryRef, branch: BranchName,
        produced: CommitRevision, observed: CommitRevision, relation: PublicationRelation },
    ChangeOpened { change: ChangeRequestRef, head: CommitRevision },
    MetadataUpdated { change: ChangeRequestRef, head: CommitRevision, fields_digest: Digest },
    CommentPosted { change: ChangeRequestRef, comment: HostCommentRef,
        actor: HostActorRef, marker: ActionMarker, body_digest: Digest,
        target: DiffTarget, thread: Option<HostThreadRef> },
    ThreadResolved { thread: HostThreadRef, target: DiffTarget },
    ReadyObserved { change: ChangeRequestRef, head: CommitRevision },
}
```

Applied means the normalized fields actually match the immutable intent, not merely HTTP success or a prose claim. A definite failure requires evidence that this effect did not occur; a transport error after a possible write is unknown. Retain append-only receipts and a guarded current projection. Applied historical facts do not turn into failed effects because a branch later advances; they become insufficient for a new target's readiness.

Read-only reconciliation uses the original ActionId/payload/actor/binding, remote identity or marker and complete enough authoritative reads. Adopt exactly one matching effect; ambiguity or unavailable reads stays unknown. Timeout cannot convert unknown to failure. It never resends a write as a side effect of reading. A new action after a definite failure requires a fresh Core decision, satisfied prerequisites and bounds; no generic automatic reissue. Stable local action IDs do not make SQLite and GitHub one transaction or promise exactly-once remote writes.

A successful push with unknown PR creation, or posted reply with unknown requested thread resolution, remains partial. Keep successful receipts and reconcile outstanding effects independently. Cancellation/budget exhaustion prohibits new dependent writes but does not undo an already-applied effect or erase an unknown one. Current/fresh head/base reads are required for applicable writes; they are observational boundaries, not remote locks. Host conditional ref writes enforce actual prior-ref and fast-forward concurrency protection.

The narrow existing `ConvergeRecord::classify` can establish published-produced equality/ancestry. It does not validate replies, ready state, spec/invariants, current checks, full review or stop proof. An ancestor commit proves publication only; the newer current head needs its own applicable checks/review. Unchanged-head `nothing-to-fix` does not prove a reported violation repaired and does not waive outstanding effects.

## CI identity and requirement snapshot

The Work's declared verification argv is the mechanical requirement set. Additional GitHub CI is an explicit config list, normalized at acceptance into named requirement identities with repository and producer/context provenance. A display label alone cannot choose arbitrarily between conflicting producers. the [installation configuration](installation-config.md) specifies setup resolution and the [execution loop](execution-loop.md#cli-contract) specifies output; no branch-rule discovery is required.

An empty CI list is a positive policy fact: no GitHub CI dependency was requested. If a check is named, keep its expected producer/context, target change/head, current run/attempt identity and conclusion. Use the latest authoritative applicable attempt; do not cherry-pick an older success over a current failure/pending run. A successful unrelated producer or another change with the same SHA does not satisfy it. Pending, failed, skipped/neutral or unavailable does not equal successful. This initial profile requires success for each named check; adding alternate conclusions is outside this design.

Repository merge-rule changes are not automatically imported into Work policy. New config cannot silently remove accepted requirements; an explicit approved policy/specification update or new Work must be designed before changing an existing one. Retain the exact requirement snapshot in every assessment and Outcome.

## Structural storage and transactions

Add immutable typed evidence metadata/content references, review snapshots/rounds/occurrences/assessments, invariant assessments and required-stage-effect records to #50's model. Foreign keys bind exact Store/Work/Execution/specification/policy and diff targets; content references may be compacted/moved under #54 without changing metadata identity or fabricating availability. Do not keep raw unbounded remote text as keys.

Run `python3 scripts/check-evidence-schema.py` to check the fragment's membership/uniqueness constraints and same-revision recollection with stub subjects. It does not prove exact-set transaction predicates, sealed-row immutability, recovery semantics, implemented store behavior, or live acceptance.

The following SQL fragment illustrates exact-set membership constraints; parent subject/source tables and the full target-scoped FKs remain part of the complete schema implementation, not omitted checks at runtime.

```sql
CREATE TABLE review_rounds (
    id TEXT PRIMARY KEY,
    subject_id TEXT NOT NULL REFERENCES assessment_subjects(id),
    set_digest TEXT NOT NULL,
    sealed INTEGER NOT NULL CHECK (sealed IN (0,1)),
    UNIQUE (id, subject_id)
);
CREATE TABLE source_snapshots (
    id TEXT PRIMARY KEY,
    subject_id TEXT NOT NULL REFERENCES assessment_subjects(id),
    source_id TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    completeness TEXT NOT NULL CHECK (completeness IN ('complete','partial','unavailable')),
    payload_ref TEXT NOT NULL,
    UNIQUE (id, subject_id, source_id),
    UNIQUE (subject_id, source_id, collection_id)
);
CREATE TABLE review_round_sources (
    round_id TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    PRIMARY KEY (round_id, source_id),
    UNIQUE (round_id, snapshot_id),
    FOREIGN KEY (round_id, subject_id) REFERENCES review_rounds(id, subject_id),
    FOREIGN KEY (snapshot_id, subject_id, source_id)
        REFERENCES source_snapshots(id, subject_id, source_id)
);
CREATE TABLE review_occurrences (
    round_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    occurrence_id TEXT NOT NULL,
    source_finding_key TEXT NOT NULL,
    PRIMARY KEY (round_id, occurrence_id),
    UNIQUE (round_id, snapshot_id, source_finding_key),
    FOREIGN KEY (round_id, snapshot_id)
        REFERENCES review_round_sources(round_id, snapshot_id)
);
CREATE TABLE round_judgments (
    id TEXT PRIMARY KEY,
    round_id TEXT NOT NULL REFERENCES review_rounds(id),
    set_digest TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    payload_ref TEXT NOT NULL,
    UNIQUE (id, round_id)
);
CREATE TABLE judgment_decisions (
    judgment_id TEXT NOT NULL,
    round_id TEXT NOT NULL,
    occurrence_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN ('fix','dismiss','accept','wait')),
    PRIMARY KEY (judgment_id, occurrence_id),
    FOREIGN KEY (judgment_id, round_id) REFERENCES round_judgments(id, round_id),
    FOREIGN KEY (round_id, occurrence_id) REFERENCES review_occurrences(round_id, occurrence_id)
);
```

Source revision identifies the provider report/content version, while collection_id identifies one read/collection attempt. A partial/unavailable collection remains immutable; successful recollection of the same provider revision is a new collection and can produce a complete snapshot. Repeated identical delivery of one collection is idempotent; conflicting bytes under that collection identity are preserved as conflict. Two complete collections with the same stable report revision but contradictory report content cannot silently replace each other. Identical complete contents need not advance the material gate generation.

Seal only after the configured requested source is complete and the canonical occurrence set is stored. Reject update/delete/post-seal child insertion; new facts create a replacement snapshot/round. The composite snapshot FK includes source_id, so a round cannot claim another source's snapshot. Judgment insertion, exact coverage test in both directions, actor/rationale validation, pointer/WorkVersion update and saved Command result commit atomically. SQL membership prevents cross-round decisions; exact coverage and current-generation checks remain explicit Core transaction predicates. A body reference or set digest alone is not proof of semantic coverage.

An enabled invariant assessment has a parallel immutable set-coverage constraint over `(subject, invariant_set, invariant_id)`, not the code-review occurrence set. Separate flags cannot accidentally share a `review_passed` boolean. Outcome references typed evidence, not an empty untyped list. Work's current assessment pointers are derived control state; immutable records retain what was actually judged. Duplicate IDs with different canonical bytes become evidence conflicts, not updates to sealed records.

## Outcome, counters, and repair

```rust
struct CompletionProof {
    subject: AssessmentSubject,
    current_target: DiffTarget,
    artifact: ArtifactId,
    mechanical: Vec<VerificationEvidenceRef>,
    invariant_check: LaneProof<InvariantAssessmentRef>,
    code_review: LaneProof<ReviewCompletionRef>,
    configured_ci: Vec<NamedCheckEvidenceRef>,
    required_effects: RequiredStageEffectsRef,
    applied_effects: Vec<AppliedEffectEvidenceRef>,
    stopped_writers: Vec<StopEvidenceRef>,
    guard_generation: EvidenceGeneration,
}
```

Core validates nonempty evidence where the declared requirement set is nonempty, exact target/specification/profile identity, complete enabled lanes, explicit disabled markers, no current unresolved finding judgment, and closure of every required effect and writer. A normal `succeeded` Outcome points to the frozen proof with exact PR/head/base and all relevant evidence IDs. Partial progress/Attention is not succeeded. Later remote changes update current readiness, never rewrite the historical Outcome.

All real coding, fix, invariant-check, and code-review Invocations count toward per-Execution `max_invocations`. Code-review Invocations also consume `max_review_rounds`; a snapshot refresh and supervisor mechanical verification do not. A durable request consumes the attempt once, including a definitely not-started result. Replaying the request cannot charge or launch it twice. Resume, reapproval, and restart preserve usage. A successor Execution receives its own per-Execution limits while the [Work-wide recovery ledger](#work-wide-recovery-accounting) preserves cumulative correction, transport, equivalent-failure, and time bounds. No new Execution or specification revision replenishes those bounds. Artifact recovery and host transport/reconciliation remain distinct from Worker Invocations.

When enabled conformance or mechanical verification fails, no new publication is authorized. Bounded fixes produce new immutable artifacts and fresh results. Pre-publication Fix requests bind the predecessor artifact and exact failed mechanical/invariant evidence without requiring a change-request identity. Review Fix requests additionally bind the current DiffTarget, round, exact-set judgment, and selected occurrences. The approved repair goal and constraints are immutable in either case; a stale target invalidates dependent dispatch. If attempts/limits cannot resolve it, record the failed/inconclusive requirement and current Attention. Optional code-review findings similarly run bounded fix/re-review; remaining findings need an explicit operator decision. Neither budget exhaustion nor review-off selection retroactively creates passed evidence. An ordinary code-review Accept/Dismiss does not waive a required invariant violation; changing an invariant or enabled requirement needs explicit specification/policy reapproval, not a finding-status shortcut.

A repair wait/Attention carries typed prerequisites: exact failed/missing source or requirement, evidence to be refreshed, required actor/action, spec/binding to preserve, remaining limits, and any unresolved writer/effect. A resume decision rechecks the prerequisites rather than relying on a generic message `fixed`. Inconclusive/unavailable evidence can be recollected within bounds; corrupt/wrong-subject claims remain invalid history and cannot be upgraded by relabeling. A valid negative result needs a new result/remediation or explicit policy decision, not repeated parsing.

Before safe pause, prove writers stopped or unable to write and persist WIP, continuation, and resource/Issue ownership. Release compute capacity promptly once writer safety is proven, including when only a host effect remains unknown. Keep that action and its affected-resource reservations, and prohibit dependent writes and terminal settlement. Unknown process ownership retains the slot. Only safe terminal settlement releases the remaining reservations and Issue claims. Deadlines establish no stop or no-effect proof. These independent capacity and resource rules follow [human waiting and capacity](recovery-policy.md#human-waiting-and-capacity). Stop request, heartbeat silence, parent exit or worker `done` alone is never proof over all recorded descendants. The supervisor remains responsible for those process facts; Core authorizes the transition, CLI drives it.

If a human merges/closes the PR while review/fix is in progress, do not treat merge as proof of Work success or continue writing blindly. A fresh closed/merged host fact prevents dependent new mutations, retains current safety reconciliation and surfaces the appropriate operator handoff. Do not rewrite fixed Invocation inputs or infer stop proof from PR state. The [execution loop](execution-loop.md#command-surface) specifies the precise operator command/lifecycle presentation using these guards.

## Failure classification and recovery decisions

Classification consumes normalized boundary facts and exact retained evidence, not a Worker summary or a raw exit code. The configured classifier version and normalization profile are part of the accepted policy. Preserve raw bodies, classification inputs, and reasons. An exit code does not establish writer stop, artifact validity, or host-write outcome.

Apply safety precedence before choosing remediation. A request that may still launch or a writer whose state is unknown requires process reconciliation. A host write without an exact applied/no-effect result requires effect reconciliation. Neither uncertainty can become an ordinary failed attempt. After these safety checks, distinguish the following classes.

| Class | Required evidence and permitted action | Refusal and handoff |
| --- | --- | --- |
| `transient` | A versioned classifier identifies a recoverable read/service failure, or a write has exact no-effect proof. Retry the same logical operation within saved backoff, transport count, and deadlines, rechecking fresh write guards when exact no-effect proof permits another write attempt; allocate a new counted attempt only after the decision commits | Missing no-effect proof becomes `unknown_effect`. Exhaustion holds for human action; never invent an applied receipt |
| `permanent` | Exact request plus deterministic refusal, unsupported operation, or invalid task. Retain the refusal; stop safely and fail the Execution when no writer/effect can act | Do not automatically buy the same refusal again. Resume/re-execution requires changed validated input or an explicitly approved policy/specification decision |
| `environment_repair` | Auth, permission, configuration, sandbox, or durable-store prerequisite failed. Hold for repair, save exact prerequisites, safely stop, and release compute capacity when proven safe | A generic acknowledgment or unchanged config cannot resume. Require a fresh successful boundary check tied to the repaired credential/config/environment revision |
| `human_decision` | A finding needs disposition, repeated correction failed, a source changed/closed, or a configured bound is reached. Persist current Attention and continuation | No automatic acceptance or retry. An authorized decision must name the current Work version, evidence, and remaining limits |
| `unknown_effect` | The original ActionId/request, authorized actor/target/payload, partial facts, and lost/ambiguous result exist. Inspect only the original action and its exact postcondition within the reconciliation window | No blind resend, successor write, success, or terminal release. Expiry requests human action and preserves uncertainty; stopped-writer capacity can still be released |
| `correction_required` | A valid exact mechanical/invariant negative result or actionable current review decision identifies a code defect. Request a new immutable fix Invocation and fresh verification within all applicable bounds | Do not use transport retries to repeat failing checks or accept stale success. Missing evidence is not a code defect |
| `unknown_evidence` | Classification, report, or artifact evidence is absent, incomplete, invalid, or unclassifiable. Preserve its validity and reason; bounded recollection may obtain a new exact record after writer stop | Never interpret unknown as changed cause or success. Invalid evidence remains immutable. Exhaustion holds for repair/human action or safely fails with the unresolved requirement recorded |

A valid negative result and its normalized class are separate fields. For example, a valid failed check may require environment repair rather than code correction. A missing report with a valid handed-off artifact does not fabricate review/invariant results. Unavailable source bodies block dependent gates even if a previous digest is known. A stale or wrong-subject record cannot be repaired in place; record a fresh collection for the intended subject.

```rust
struct FailureRecord {
    id: FailureId,
    subject: AssessmentSubject,
    operation: RecoveryOperationRef,
    attempt: AttemptRef,
    class: FailureClass,
    reason: ReasonCode,
    evidence: NonEmptyVec<EvidenceId>,
    evidence_state: EvidenceValidity,
    classifier: ClassifierVersion,
    normalization: NormalizationProfileRef,
    equivalence: FailureEquivalence,
    condition: ConditionSnapshotRef,
}
enum FailureEquivalence {
    Known { stage: StagePurpose, criterion: StableCriterionRef,
        category: ErrorCategory, diagnostics_digest: Digest },
    Unclassified { reason: ReasonCode },
}
struct RecoveryDecision {
    id: RecoveryDecisionId,
    work: WorkRef,
    expected_version: WorkVersion,
    failure: FailureId,
    policy: PolicySnapshotRef,
    action: RecoveryAction,
    prerequisites: Vec<RepairPrerequisite>,
    usage_before: RecoveryUsageRef,
    usage_after: RecoveryUsageRef,
    authority: DecisionAuthority,
    reason: ReasonCode,
    decided_at: Timestamp,
}
enum RecoveryAction {
    Recollect { source: EvidenceSourceRef },
    Reconcile { original: RecoveryOperationRef },
    RetryTransport { original: RecoveryOperationRef, not_before: Timestamp },
    Correct { predecessor: ArtifactId, request: RequestId },
    Requeue { predecessor: ExecutionId, successor: ExecutionId },
    Hold { continuation: ContinuationRef, attention: AttentionRef },
    Stop { intent: StopIntentRef },
    Fail { outcome: OutcomeRef },
}
struct RepairPrerequisite {
    criterion: RepairCriterionRef,
    prior_condition: ConditionSnapshotRef,
    required_actor: ActorRequirement,
    required_evidence: EvidenceRequirementRef,
}
```

These types extend the target design, not the implemented Rust API. `RecoveryOperationRef` resolves to a scoped source collection, artifact recovery, Invocation request, or host action. `AttemptRef` distinguishes a new attempt from delivery replay. `ConditionSnapshotRef` identifies immutable input/config/capability/check/source revisions and their retained validation evidence. Evidence validity records absent, valid, or invalid with an explicit reason; unavailable and incomplete collections retain their source state. A known equivalence key uses canonical bytes under the saved normalization profile. Scope the key to Work, stage, stable criterion, error category, and normalized diagnostics. Strip only the profile's declared volatile fields such as timestamps and temporary paths. Never merge by free-text similarity or delete the raw evidence.

Record every authoritative classification once per operation/attempt/evidence revision. Re-reading a failed check is not a second failed validation. A fix artifact or new Execution alone does not change equivalent-failure identity. Unclassifiable diagnostics cannot reset a known streak or justify another automatic correction; hold for classification or human action. A changed classifier version is an explicit policy update, not a reset signal. Preserve comparable prior usage; if the versions cannot be compared, require human decision rather than assuming a new failure.

The Core transaction stores the FailureRecord, decision, prerequisite evidence, usage delta, next not-before/deadline, continuation or request, and WorkVersion update together. Decision identity and attempt uniqueness make delivery replay idempotent. A version conflict rolls back the entire decision and requires rereading current facts. No adapter may allocate a successor Execution or repeat an external write on its own. Store immutable records plus Work-current pointers; retain actor and old/new policy for any authorized change. SQL uniqueness must cover `(work, operation, attempt, classification_revision)` and `(work, decision_id)`, with Work-scoped foreign keys for failure, usage, and successor lineage. Duplicate identity with different canonical bytes is a conflict.

A repair prerequisite is satisfied only by valid fresh evidence for its exact criterion and subject. A changed token identifier without a successful permission check is insufficient. A changed spec needs explicit approval; a new policy keeps old usage. Reject absent, stale, wrong-actor, malformed, or mismatched evidence with a retained reason. An explicit operator retry after safe failure creates a successor only if its saved prerequisites and all applicable bounds pass; approval cannot serve as writer-stop or no-effect evidence. Budget expansion uses the approved policy-change path, never a caller-supplied fresh counter baseline.

## Work-wide recovery accounting

Use the numeric defaults already approved in [recovery policy](recovery-policy.md#configurable-policy-and-defaults). This design adds no numeric default or exception. Each effective policy is immutable and versioned; configuration edits affect new Work unless an authorized version-checked policy change updates existing Work. Store counters as Work-owned ledger entries and cached totals, not only fields on the current Execution.

| Account | Durable identity and charge | Exhaustion behavior |
| --- | --- | --- |
| Invocation | `(Execution, RequestId)` counts coding, fix, invariant-check, and code-review requests once, including definitely not-started attempts; preserve historical per-Execution totals | No new Invocation after that Execution's limit; safely fail/hold under the selected recovery decision |
| Review | `(Execution, code-review RequestId)` also consumes one review round. Polls, snapshot recollection, supervisor checks, and invariant checks consume no review round | Human decision without automatic finding acceptance |
| Automatic correction | Work-wide fix-request identity. Charge once when Core durably authorizes an automatic fix, whether caused by code, checks, invariants, or review | Default three automatic fix Invocations across all Executions; a successor or reapproval cannot add a fourth |
| Transport | `(Work, logical operation, retry sequence)` counts retries after the initial attempt. The logical operation retains its identity across restarts and successor Executions; an ActionId cannot be replaced to replenish retries | Default three retries with saved `[10,30,90]` second backoff. Recheck remaining deadlines before dispatch; host-write uncertainty switches to reconciliation |
| Artifact recovery | `(Work, ArtifactId or producing Invocation, recovery sequence)` and original recovery deadline; a successor cannot restart recovery for the same artifact | Existing default three attempts within thirty minutes. A newly produced artifact has its own recovery account and still consumes Work automatic time |
| Equivalent validation failures | `(Work, equivalence key)` plus current correction/validation lineage. Charge each new authoritative failed post-correction validation once; retain all historical counts and the consecutive streak | At the approved default of two consecutive equivalent post-correction failures, request human action. A matching check becoming valid and successful ends its streak; an explicit different classified cause starts its own streak. Neither resets other usage |
| Execution runtime | First-dispatch timestamp and saved per-Execution runtime deadline, including pauses after first dispatch; queue time before first dispatch does not start it | Existing default six hours; resume never moves its deadline |
| Automatic time | Work-wide non-overlapping intervals, opening at first dispatch and while automatically running or waiting, including backoff/reconciliation and automatic queue waits after first dispatch | Default four cumulative hours. Human-decision waits are excluded only after their durable handoff. Earlier automatic time remains charged |
| Absolute Work deadline | Optional immutable Work deadline, compared with observed time in every relevant decision; continues through all queues and human waits | No attempt after deadline. Stop/reconcile safely, never infer termination from clock expiry |

Maintain `(work, usage_sequence)` immutable deltas with a unique causal decision/request/attempt reference; cached totals and successor creation commit in the same transaction. Every automatic successor inherits the Work's policy, cumulative usage, failure history, pending operations, and original applicable deadlines. Per-Execution counters may start at zero for the new Execution, but do not replace these Work accounts. A successor request correcting an earlier produced change remains a correction and consumes the Work fix account even if its adapter task uses the coding shape. Creating E2 or relabeling the role cannot turn correction into a new initial implementation. Human resume uses the same paused Execution and reacquires capacity through the queue. An operator retry creates a new Execution only after safe failure and fresh guards; none of these commands refund usage.

Time accounting records each phase boundary and one current open interval. Concurrent CI and reconciliation consume one wall-clock interval, not the sum of lane times. A restart closes/reopens no artificial free interval: charge elapsed time since the durable boundary before authorizing work. Use nondecreasing persisted observed time; an invalid or backward clock prevents deadline-dependent new dispatch until reconciled rather than replenishing time. Human waiting excludes only the interval durably classified as requiring human action, not unrecorded downtime. Per-Execution runtime and the absolute deadline remain independently applicable during that wait. Policy changes preserve elapsed intervals and counters; tightening below consumed usage takes the limit path, while expansion needs explicit approval.

Optional provider token/cost usage retains its source, unit, precision, and availability separately from deterministic request counts. Missing or interrupted usage is unknown, never zero. If an accepted policy uses a spend bound, unknown usage cannot establish sufficient remaining allowance; reconcile it or request a policy-authorized human decision. No new spend default is introduced here.

Material progress is a separately persisted versioned observation. Accepted new artifact evidence, a formerly failing required check becoming valid and successful, or a reconciled action acquiring a verified result may rearm the no-progress warning. A log message, new commit hash, partial report, repeated poll, or new Execution cannot do so by itself. The default thirty-minute warning does not force a stop, waive an unresolved effect, or refund any attempt/time. Each occurrence retains the evidence and the configured progress rule.

| Literal case | Accepted or rejected result |
| --- | --- |
| W1/E1 used two automatic fixes; E2 requests another | Accept the third only if other guards/bounds pass. Persist W1 usage three and E2's own Invocation charge |
| E2 requests a fourth automatic fix after W1 usage three | Reject exhausted Work bound even with unused E2 Invocation slots |
| Correction C1 fails exact check T with key K; C2 fails T with only timestamp/path changes | Two distinct post-correction validations under K; Attention. Duplicate delivery of C2 does not add a third |
| New head/report has unclassifiable diagnostics | Preserve unknown; cannot assert a different cause, clear K, or automatically re-execute |
| T becomes valid and passes after explicit repair | Reset T's consecutive streak with retained evidence; retain previous failures, correction usage, and automatic elapsed time |
| Restart after transport retry two during backoff | Preserve operation identity, consumed retries, and not-before time. The next retry is three, not one |
| One hour automatic work, two hours human wait, then resume | Automatic time remains one hour; Execution runtime has elapsed three hours and absolute deadline continues |
| Operator retries with unchanged failed environment | Reject missing changed-and-validated prerequisite; a new Execution is not repair evidence |
| Approved extension raises fix limit from three to four after usage three | Preserve usage three and both policy versions. One more automatic fix may become eligible; no refund or other invariant waiver |
| Unknown create action exceeds ten-minute reconciliation window | Attention with original ActionId and partial push receipt; no new create, failed receipt, or terminal settlement |

## Incident contracts and regression scenarios

The source inventory and [requirement obligations](evidence-requirements.md) provide traceable design inputs. Source fixes establish a failure class, never target-runtime acceptance. Keep private source locators only in the owner-controlled audit record. In particular, the existing #21/PR #31 classifier evidence supports only its recorded classification assertion, not the complete side-effect gate or a live publication claim.

| Incident | Exact contract and retained state | Planned regression through Command/Query and adapter ports |
| --- | --- | --- |
| Publication claimed without push | Worker report has no host-effect authority. Require independent exact push receipt before open/update or any reply claiming publication; keep missing versus invalid receipt reasons | Supply a valid local artifact and a report saying published, but no ref receipt. Assert no create/reply authorization and no succeeded Outcome. Repeat with a valid receipt for another commit; reject mismatch |
| Push succeeds, report or create response is interrupted | Preserve exact push result and original action. Missing Worker report drives bounded evidence recovery; unknown create drives reconciliation, not another push/create | Persist applied push, lose create response, restart. Assert one push and one create attempt; reconciled exact create completes its action. An unavailable/mismatched read leaves it unknown. Proven writer stop releases capacity for independent Work, retaining conflicting reservations |
| Stale source head or changed diff base | Old evidence remains historical; replace current target/generation, invalidate dependent judgment/release/publication authority. Fresh pre-dispatch source read remains mandatory | Inspect H1/B1 then observe H2/B1 or H1/B2 before judgment and release. Assert rejection of old coverage and no stale-dependent effect. A late H1 notification cannot restore it |
| Same-head finding growth | An exact complete round `{A}` cannot authorize decisions over `{A,B}`. Install new immutable round and invalidate dependent generation in the same local transaction | Race collection of B with judgment acceptance and terminal release on H1. Assert either valid old transaction before new evidence or explicit stale/set-mismatch rejection after it; never accept incomplete decisions. Duplicate delivery leaves generation unchanged |

These planned regressions exercise local transactional guarantees and the recorded final-read boundary. They do not promise an atomic remote snapshot or eliminate a remote change after the last authoritative read. Each required evidence, FailureRecord, RecoveryDecision, usage delta, prerequisite, and completion proof must be tested as absent, exact-and-valid, or malformed/stale/mismatched, with its original reason retained.

## Literal acceptance and rejection cases

| Scenario | Decision / retained evidence |
| --- | --- |
| Invariant check off, code review off, no CI names | Only declared mechanical/pre-publication/effect/stop gates apply. Proof records both disabled lanes and CI-empty policy, never invented positive reports |
| Invariant check on, exact complete positive coverage | May authorize publication after mechanical success and stop/artifact validation |
| Exact checker result reports a violation | Valid negative evidence; fix/recheck within budget, otherwise Attention, no new push |
| Checker result says `all good` but omits invariant I2 | Incomplete coverage; cannot pass or publish |
| Code review on, complete requested source with zero findings | Review may pass for the exact head/base/spec/policy. No native Approve required |
| Same head gains B after judgment inspected A | New round/generation; old judgment or release rejected even if caller guessed current WorkVersion |
| Judgment includes A but omits B, duplicates A or includes foreign C | Reject exact-set mismatch; no partial judgment/next-stage authorization |
| Head same, target/base moved | Old diff positions/review stale; retain history and request current assessment |
| Named CI `lint` from producer P is absent | Wait/unavailable; unrelated producer Q's success or another head cannot satisfy it |
| No CI checks configured but repository has failing optional checks | Do not silently add Work requirements. Retain external facts separately; repository merge policy is unchanged |
| Push applied, create response lost | Preserve push receipt; reconcile original create only, unknown prevents full publication/terminal release |
| Reply claims fix published before matching push evidence | Reject prerequisite; report text cannot prove publication |
| Review budget exhausted with findings | Attention/human handoff, no auto-accept, no native merge-blocking action |
| Outcome has passing old head but current head changed | No current completion authority; old proof stays historical |
| Source/check/report is absent, valid negative, invalid, or unavailable | Preserve those distinct states and reason; none becomes success from empty output |

## Six aspects and verification scope

| Aspect | Applicability / rationale | Planned proof and remaining work |
| --- | --- | --- |
| State transitions | New evidence competes with judgments, requests and terminal commit | Two-connection exact-set races at judgment/release/dispatch; no partial accepted effect set or stale proof |
| Failure semantics | Negative, invalid and unknown are different; partial effects survive | Per-lane absent/positive/negative/invalid/unavailable matrix; lost-response push/create/reply, safe repair/terminal guards |
| Trust boundaries | Worker/checker/source claims never directly settle Work | Wrong producer/actor/Work/spec/repo/head/base, malformed envelope and forged complete-empty source tests; no raw provider JSON logic in Core |
| Identity invariants | Same source IDs, heads and findings recur across attempts | Scope collisions, duplicate conflicting payloads, immutable round/assessment, cross-attempt links without inherited decisions |
| Information preservation | Original judgments/results/partial successes must survive new rounds | Compare retained exact snapshots, references, rationale and invalidity reasons; reclaimed required body never replaced by hash-only success |
| Resource bounds | Two optional LLM lanes and repeated fixes consume real budget | All Invocations counted once; Work-wide recovery usage survives successor Executions, resume, reapproval, and restart; independent invocation/review/artifact/transport/time bounds compose; the [installation configuration](installation-config.md) and the [execution loop](execution-loop.md) expose policy and Commands |

Implementation tests must call the public Command/Query/adapter path and assert literal decisions/evidence, with direct structural FK/immutability tests where appropriate. Cover all four on/off lane combinations; same-head growth before judgment and between judgment/release; exact invariant-set coverage; valid negative versus invalid records; named CI producer/head/rerun selection; wrong diff base; no native merge gate; original-action reconciliation; no repeated effects; process escape/uncertain stop; proof closure with missing bodies; budget accounting and paused repair. These are future contract tests, not implemented fixes to #24 or live evidence.

The candidates compared a mutable per-finding disposition ledger and immutable round/source/assessment snapshots. The latter is the base: it records exactly which complete set was judged and makes same-head growth explicit. Take the former's concise partial-effect/repair tracking, but not text-based finding closure or generic write retry. Work-wide recovery budgets follow the later owner-approved #50 policy and supplement per-Execution limits. Small immutable assessment vectors and typed current pointers avoid a generic workflow/finding graph. #52 owns Worker protocols, #53 owns config/credentials/defaults via the [installation configuration](installation-config.md), #54 body/history retention, #55 CLI/loop/repair syntax via the [execution loop](execution-loop.md), and #56's [design closure](design-closure.md) integrated acceptance. The owner-approved choices above are not reopened by those implementation details; the code-review lane default is resolved as `on` in the [design closure](design-closure.md#owner-decisions). No runtime work is authorized by this document.
