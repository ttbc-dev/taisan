# Extracted evidence and recovery requirements

Status: normative target obligations for Issue #51, derived from the complete pinned inventory in [extraction coverage](extraction-coverage.md). The schemas and transition predicates are specified in [evidence gates](evidence-gates.md), and numeric defaults and approved scope decisions come from the merged [recovery policy](recovery-policy.md) and [identity/storage design](identity-storage.md). This is documentation only. No requirement below claims runtime implementation, a fixed defect, live host behavior or executed acceptance.

## Reading and evidence rules

Each R heading is a precise target for the #51-owned inventory link. The scope is that requirement's evidence, assessment or recovery obligation, not its entire source mechanism. Other owner targets under #52 through #56 remain separately planned. Source-specific orchestration, autonomous judgment, evaluation products, automatic merge and automatic conflict repair are not introduced by these mappings; their retained safety obligations are stated below.

Apply this validity rule to every required record, body, artifact, receipt, counter and decision below:

| Evidence state | Required behavior |
| --- | --- |
| Absent or unavailable | Retain the exact missing identity and reason. Hold the dependent gate, request bounded recollection or repair when permitted, and surface Attention at its policy boundary. Never treat missing evidence as an empty complete set or a passed requirement. |
| Present and valid | Validate declared schema, producer, mode, Work, request, specification, policy, repository, target revision, digest and required coverage. A valid negative or pending result remains negative or pending; only its actual postcondition may satisfy a gate. |
| Present and invalid | Retain the original identity and precise malformed, stale, mismatched or conflicting reason. Refuse dependent authority, obtain a new valid record under the correct identity when allowed, and never relabel the old record as valid. |

Retained content is subject to #54's secret-safe storage and dependency pins. An authorized policy change preserves counters and historical evidence. Unknown writer or effect ownership keeps its safety holds even when a deadline expires. A stopped writer's compute slot is distinct from its still-reserved affected resources.

The proof paragraphs are planned contract scenarios. Run them through the future public Command, Query and adapter paths with literal expected decisions, retained evidence and effects. Structural schema checks supplement those scenarios; they cannot establish live behavior. This document's 125 requirement headings cover all 125 #51 inventory targets at this revision. Counts can be regenerated with the extraction checker and by counting targets whose `owner_issue` is 51 in `docs/extraction-coverage.json`.

## R03

Repository policy evidence. Bind allowlist, repository, actor, capability and lane requirements to the accepted policy snapshot. Missing or unverifiable policy evidence holds dependent publication; changing installation defaults cannot silently alter this Work.

Planned proof for AC1: Load a policy for another host with the same repository display name and reject it; change defaults after acceptance and show that publication still requires the saved policy. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R04

Semantic request and evidence identity. Separate immutable semantic input from mutable collection and display bookkeeping. An identical Command replay returns its original decision; changed specification, actor or payload under the same identity is a conflict rather than a replacement assessment.

Planned proof for AC1: Replay equal input after collection bookkeeping changes and get the same saved result; change task bytes under the same key and assert a conflict with no new Invocation. See the [governing contract](evidence-gates.md#structural-storage-and-transactions).

## R08

Unknown observations and repair prerequisites. Missing scans, failed probes and unreadable source records are unknown, never free capacity, absence of findings or evidence of repair. Resume requires the exact failed prerequisite to acquire new valid evidence and still satisfy final admission guards.

Planned proof for AC1: Inject a scan outage and a lost launch acknowledgement, then show that neither creates a replacement writer; replay the unchanged repair evidence and refuse resume. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R09

Evidence for capacity release. Capacity release requires proof that a prepared request is revoked or cannot still launch, and that any recorded writer stopped or cannot write. A stopped writer's unknown host effect retains the affected resource reservations and reconciliation identity, while its compute slot may be released.

Planned proof for AC1: With capacity one, stop W1 and leave its create action unknown: independent W2 runs, conflicting W3 remains excluded, and W1 cannot settle. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R10

Atomic dispatch evidence. Save the request, transition, resource ownership and applicable budget charges atomically before launch. A persistence error authorizes no launch, and an ambiguous launch reconciles its original request instead of creating a fresh one.

Planned proof for AC1: Fail request persistence and assert zero launches; crash after the durable request but before acknowledgement and assert exactly one reconciled Invocation. See the [governing contract](evidence-gates.md#structural-storage-and-transactions).

## R11

Lost acknowledgement and successor limits. An absent launch or write acknowledgement records uncertainty under the original request or ActionId. New Command keys and successor Executions cannot bypass unresolved ownership or restore Work-wide recovery usage.

Planned proof for AC1: Lose a start response and keep the same Invocation; split equivalent failures across successor Executions and assert one cumulative limit and no extra dispatch. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R12

Proof of a single writer. Workspace and branch ownership derive from scoped durable reservations, writer fencing and the supported permission boundary. Remote labels, a local lock or an expired lease alone cannot prove exclusive ownership or safe takeover.

Planned proof for AC1: Race two supported callers and obtain one writer; remove its remote label or expire a display lease and prove that a second writer is still refused. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R13

Immutable bounded Invocation. Each request fixes task, base, verification requirements and execution limits before dispatch. Core applies per-Execution and Work-wide limits together, counts durable requests once and cannot reset usage through reapproval or restart.

Planned proof for AC5: Restart between charging and launching a request, then assert the saved charge is counted once; exhaust the Work bound across two Executions and refuse another correction. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R14

Worker claims and Core Outcome. A validated Worker report is evidence for its exact request, never authority to declare Work success. A completed report still needs retained artifact, verification, publication and writer-safety proof; malformed or absent reports cannot become empty success.

Planned proof for AC1: Return a completed report with missing publication and stop evidence and assert no success or terminal release; retain its report and missing-evidence reason. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R15

Stop proof and uncertainty. Persist stop intent before signaling and identify every recorded descendant with reuse-resistant process identity. Missing liveness observations, a dead leader or a worker summary cannot prove the process tree stopped.

Planned proof for AC5: Fail intent persistence and assert no signal; leave a live child after leader exit and retain the slot; inject a process-probe outage and retain uncertainty. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R16

Optional planning evidence. A separate planner remains optional Worker-internal hardening rather than a required v0.1 port. If planning evidence is used, bind it to the immutable request and distinguish a claimed read-only phase from enforced permissions; a plan cannot satisfy verification or review.

Planned proof for AC1: Run without a planner under the approved profile; supply a plan as test evidence and refuse it, and reject a plan bound to a different head. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R17

Exact retained change and verification. Bind verification to retained git objects, produced commit, content digest, command argv and environment identity. An ephemeral workspace path, a hash without required bytes or results from another artifact cannot satisfy this change's requirement.

Planned proof for AC1: Remove required object bytes after a timeout and deny completion while preserving recovery references; alter the patch digest or verification argv and reject the old result. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R18

Typed failure classification. Classify structured request, evidence and stop facts independently from process exit status. A known refusal, incomplete collection, valid negative check and unknown write need distinct persisted reasons and different allowed recovery actions.

Planned proof for AC5: Return exit zero with a human-decision report, and a nonzero transport error after an ambiguous write; neither becomes success or a blind replacement attempt. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R19

Risk gates and approved lanes. The separate high-risk manifest mechanism remains deferred. Enforce the approved specification, mechanical checks, enabled invariant lane and configured review policy without silently introducing that extra gate or waiving existing requirements.

Planned proof for AC1: Omit the deferred manifest and still enforce enabled invariant coverage; supply a risk summary claiming success and show it cannot replace any required check. See the [governing contract](evidence-gates.md#owner-approved-lanes-and-caller-flow).

## R20

Bounded corrective verification. A failed required check may cause a counted correction on its exact retained predecessor artifact. Every changed artifact repeats mechanical and enabled invariant verification; environment repair and unavailable evidence follow their own recovery classes.

Planned proof for AC1: Fail a check, dispatch one correction and verify its new artifact; exhaust the Work correction bound and obtain Attention rather than publication or another Invocation. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R21

Publication owner and durable intents. Core persists each authorized host effect before the host adapter executes it under the saved actor and binding. Separate push, create, reply, thread and ready receipts preserve partial success, and credential renewal cannot silently change an outstanding action's actor.

Planned proof for AC1: Rotate credentials after a push intent, then verify the original actor before execution; lose create acknowledgement after a successful push and retain both the push proof and unknown create. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R22

Publication evidence for completion. A fixed or complete claim requires proof that the exact produced change reached the bound repository and change request. Equality or ancestry may prove publication, but a newer current head still requires its own applicable verification and review.

Planned proof for AC2: Claim fixed without a push receipt and deny completion; prove the produced commit is an ancestor of the current head and require fresh checks for that head. See the [governing contract](evidence-gates.md#incident-contracts-and-regression-scenarios).

## R23

Independent host postconditions. Marked reply, thread resolution and ready state are separate effects with saved targets and payload digests. No reply proves a thread resolved, no prepared body proves a post landed, and the reference workflow authorizes neither merge nor force-push.

Planned proof for AC1: Apply a reply and lose thread-resolution response, then retain partial success and reconcile only resolution; reject a merge or force update requested by report text. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R24

Bounded reasons and retained diagnostics. Persist closed failure categories and bounded sanitized display reasons with references to retained evidence. Do not discard the actual invalidity or repair cause, expose credentials in projections, or let an old repaired refusal override a newer authoritative result.

Planned proof for AC1: Inject an oversized credential-bearing error, assert redacted bounded output, and retain its evidence identity; supersede a known auth failure with fresh verified repair evidence. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R25

Exact judgment target. A judgment binds to current WorkVersion, specification, policy, head, comparison base and complete occurrence-set digest. Same-head finding growth or a changed base invalidates acceptance, release and dependent dispatch even when a caller supplies a newer WorkVersion.

Planned proof for AC2: Inspect A, add B on the same head and reject the old judgment at submission and release; move only the base and require a new assessment. See the [governing contract](evidence-gates.md#same-head-growth-and-exact-set-guards).

## R26

Bounded convergence and progress. Use policy-bounded correction and review loops, routing unresolved decisions and repeated equivalent failures to Attention. Semantic progress can rearm a no-progress diagnostic but log growth, a new commit or a new Execution cannot restore consumed budget or prove repair.

Planned proof for AC5: Produce two equivalent post-correction check failures across Executions and stop automatic repair; grow logs without semantic progress and emit a warning without claiming stop. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R27

Durable judgment history. Keep immutable rounds, occurrences, decisions and source availability across restart and successor Executions. Historical decisions are not copied onto a different target, and corrupt or incomplete ledgers cannot be interpreted as an empty decided set.

Planned proof for AC2: Restart after a saved judgment, change the head and refuse its reuse; corrupt a current snapshot and expose unavailable evidence while retaining prior judgment history. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R28

Finding relations across attempts. Persist occurrence identity and explicit predecessor relations without fuzzy text-based merging. Claim a specific fix only with its artifact, publication and reassessment chain; nonappearance in a partial review proves nothing.

Planned proof for AC2: Repeat equal finding prose under a new request and preserve distinct occurrences; remove it from a partial review and assert no fixed disposition or inherited dismissal. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R30

Read-only reconciliation. Reconciliation inspects the original process or host action using its exact identity, payload, actor and postcondition. Ambiguous matches and failed reads remain unknown, and a merged PR without request binding cannot identify which Work or effect completed.

Planned proof for AC3: Return zero and multiple marker matches, an unreadable process record and an unrelated merged PR; each leaves the original uncertainty and produces no write. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R31

Replay without new authority. Stable Command and Action identities retain their saved decisions and receipts. Replaying a terminal operation cannot retire newer requests or evidence, and a reconciliation observation cannot acquire dispatch authority from an old terminal result.

Planned proof for AC1: Commit a newer action before replaying an older terminal Command; assert the new action remains pending and the replay returns only its historical result. See the [governing contract](evidence-gates.md#structural-storage-and-transactions).

## R32

Recorded bounded recovery. Record recovery class, evidence, attempt charge and deadline before any permitted retry or recollection. Distinguish a recoverable missing artifact from corrupt or wrong-subject evidence and permanent collection refusal; exhaustion cannot erase unknown writers or effects.

Planned proof for AC3: Inject missing then corrupt artifact evidence, a permanent refusal and a crash after charging; assert distinct reasons, bounded recollection and no budget reset. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R33

Changed-evidence resume. A repair decision names the failed prerequisite, required actor or action and the evidence that must change. Resume checks fresh validated evidence, WorkVersion, binding, remaining limits and unresolved ownership rather than accepting an operator's generic fixed message.

Planned proof for AC3: Retry with identical failed auth evidence and refuse; add fresh validated repair evidence and requeue under unchanged consumed usage, while a corrupt prerequisite still holds. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R35

Actionable Attention evidence. Derive Attention from current failed or unresolved prerequisites with a version, reason and required next action. Acknowledgement changes presentation only; it cannot repair evidence, release ownership or certify completion.

Planned proof for AC5: Acknowledge an unknown-effect Attention and prove the effect remains unknown; accept new valid reconciliation evidence and derive the next Attention version from current state. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R36

Visible unknown sources. Expose missing, partial, invalid, unavailable and complete-empty sources separately with their retained reasons. One malformed reference cannot hide unresolved Work or turn a partial source list into a complete assessment.

Planned proof for AC1: Fail one page of a review and one linked evidence read, then show partial availability and deny success rather than reporting zero findings. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R37

Effective configuration evidence. Assessment and Outcome refer to the saved effective policy, actor and required capability identities. Setup or health-check claims need exact validated configuration provenance; installing files or changing defaults alone does not prove a required capability or repair.

Planned proof for AC1: Supply an installation-success message without capability proof and refuse dependent dispatch; change current config and show an accepted Work still uses its saved policy. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R38

Secret-safe evidence. Keep credential values out of argv, public output and retained general-purpose evidence by resolving opaque references only at trusted boundaries. Sanitize reports and diagnostic projections without inventing missing evidence or losing the exact action identity needed for reconciliation.

Planned proof for AC1: Seed credentials in environment and error text, then inspect argv and all exposed evidence for leakage; fail credential resolution and require repair without anonymous fallback. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R40

Work-wide automatic recovery. Apply the approved transport, correction, equivalent-failure and automatic-time limits across all Executions of one Work. Persist usage, next eligible time and policy version; disablement, exhaustion and unknown accounting are distinct, and an explicitly approved extension preserves consumed usage.

Planned proof for AC6: Split equivalent failures across successor Executions and reach one Work limit; corrupt a counter and refuse dispatch; approve a larger bound and retain the old usage and unresolved effects. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R42

Safe terminal closure. Success, failure and cancellation require proof that recorded writers are safe and outstanding effects are reconciled. Deadline or budget exhaustion prevents dependent new work but does not convert uncertainty into a failed receipt or release conflicting resource reservations.

Planned proof for AC3: Cancel during launch, unknown publication and artifact repair; preserve each unresolved identity and prove no terminal release until the required safety facts arrive. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R45

Installation recovery evidence. Installation and upgrade outcomes must identify the actual validated revision, configuration and capability evidence consumed by Work. A repeated install or repaired file does not retrospectively validate an earlier failed dispatch; setup protocol remains owned by #53.

Planned proof for AC1: Fail setup validation, repair the installed dependency and require a new capability observation; keep the old failure and saved Work policy unchanged. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R46

Retention dependency evidence. Mark artifact, review, receipt and recovery bodies needed by a live gate as retained dependencies. Reclaimed optional history is explicitly unavailable, while missing required bytes block the affected assessment; #54 owns deletion and pressure policy.

Planned proof for AC1: Reach a configured retention limit with paused WIP and an unknown action, then retain their dependencies; delete a required test body in a fault fixture and deny completion. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R54

Human judgment authority. v0.1 finding acceptance or dismissal requires an authorized operator's explicit exact-set judgment and rationale. The source policy autojudge mechanism is not imported; a model recommendation cannot provide operator authority or dispose of a partially covered episode.

Planned proof for AC1: Offer model-generated answers for all but one finding and assert no accepted operator judgment; submit the complete operator decision set and validate its actor and rationale. See the [governing contract](evidence-gates.md#same-head-growth-and-exact-set-guards).

## R55

Advisory review observations. Keep shadow or unsolicited review evidence separate from the configured required review source. Duplicate observations are idempotent, dry-run records cannot prove live effects, and no webhook/card/debounce scheduler is introduced by this mapping.

Planned proof for AC1: Deliver the same advisory twice and dispatch no fix or host write; inject simulated clean review evidence and show the live required lane is still incomplete. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R64

Structured report significance. Core interprets a schema-valid report with its request and evidence, independently from exit status. A human-decision or refused report cannot become success because the process exited zero, and exit nonzero alone does not prove a valid report false.

Planned proof for AC1: Pair exit zero with a human-decision report and assert Attention; pair a retained report with nonzero exit and require all separate success and stop evidence before settlement. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R67

No publication retry by guessed absence. Proven absence of an effect is distinct from an unknown response and never creates an unlimited publication grant. Any later write after a definite failure needs a fresh guarded Core decision under the original binding and remaining limits.

Planned proof for AC1: Return an empty incomplete lookup after a lost create response and refuse a new action; obtain a complete definite-failure observation and allow at most the newly authorized action. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R75

Non-evicting disposition history. Retain finding decisions and positive fix evidence without silently dropping old obligations at a size limit. A newer occurrence can reopen current assessment, and retained paths must resolve within the trusted evidence store rather than arbitrary report-supplied locations.

Planned proof for AC1: Replay settlement without double-counting a fix, overflow a bounded collection and surface incompleteness, and reject an evidence reference escaping the managed store. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R76

Head-bound human answers. Human answers apply only to the exact round and diff target they judged. A head change or unavailable current head requires renewed inspection, never automatic transfer of a prior Fix, Accept or Dismiss.

Planned proof for AC1: Move the head after all answers are entered and refuse release; make the live head unreadable and keep the saved answers historical without dispatch. See the [governing contract](evidence-gates.md#same-head-growth-and-exact-set-guards).

## R79

No inferred policy disposition. Severity and configured routing can select a human handoff but cannot grant implicit Accept or Dismiss authority. Policy-based autonomous judgment remains outside the reference profile; dry-run and model-generated decisions remain proposals.

Planned proof for AC1: Supply a critical finding and a model recommendation to dismiss it, then require operator judgment; simulate an otherwise complete dry-run episode and show no live decisions are saved. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R81

Change-scoped mutation fencing. All supported mutation paths share the saved repository, change-request and branch resource ownership regardless of output directory or caller. A stopped coordinator or alternate workspace cannot grant a second writer while its durable request or process state remains unresolved.

Planned proof for AC1: Race two callers and a manual release for the same change, then crash after reservation but before launch and reconcile one request without a duplicate repair. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R86

Permission-repair identity. A permission denial records its policy, source target, failed capability and repair prerequisite under the Work. Repeating identical observations does not retry indefinitely; only changed validated evidence or an authorized operator decision may re-enter admission within remaining bounds.

Planned proof for AC1: Repeat the same denied capability across restarts with no dispatch, then change and revalidate it; corrupt the diagnostic reference and keep the original repair hold. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R92

Enforcement evidence before launch. A worker launch requires proof of the actual supported permission boundary, not prompt text or a permissive flag. Missing or unverifiable enforcement refuses dispatch; repairs cannot widen the immutable actor, repository or capability binding.

Planned proof for AC1: Remove the required isolation proof and assert no worker starts; repair the boundary and revalidate it while an attempted merge or force-push remains outside the accepted operations. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R103

Complete terminal evidence. Terminal assessment covers every required source and postcondition under the exact Work, not a guessed subset of linked Issues or PRs. An unavailable read, exhausted collection bound or foreign record preserves incompleteness and the affected Attention.

Planned proof for AC1: Make all but one required record terminal and fail the last read; assert no terminal settlement, retain the partial results and expose the missing identity. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R106

Typed exact-set answers. Persist one typed operator decision per current occurrence with rationale where required, then recheck the same target and generation before release. Version conflict requires reread; moved head or changed finding set requires a new judgment rather than retrying stale answers.

Planned proof for AC1: Answer two findings independently, race an evidence update before release and reject the stale set; resubmit the complete refreshed set once without duplicating decisions. See the [governing contract](evidence-gates.md#same-head-growth-and-exact-set-guards).

## R112

Durable requests without wake authority. The saved Core request is the launch authority; a timer or wakeup merely asks the CLI to reconcile it. External detached-waiter machinery is not imported, and missing wake delivery cannot erase a durable pending request or permit another one.

Planned proof for AC1: Crash after saving a request but before scheduling its wake, restart and reconcile exactly one Invocation; deliver duplicate wakes and observe no extra launch. See the [governing contract](evidence-gates.md#structural-storage-and-transactions).

## R113

Charged bounded assessment retry. A retryable assessment failure records a durable charge before scheduling another attempt and rechecks current target, policy and remaining Work budget. Corrupt accounting or a changed target cannot be treated as a free retry or restoration of an old per-head allowance.

Planned proof for AC1: Crash after charging but before dispatch and retain the charge; move the head before retry and require new current assessment without resetting Work usage. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R114

Review events are observations. A posted-review receipt can trigger observation of the configured source but cannot itself approve findings or resume a human hold. Repeated delivery is idempotent and failed or unbound publication cannot claim that a review landed.

Planned proof for AC1: Replay the same published-review event and install no duplicate round or request; deliver it during an explicit human hold and keep dispatch prohibited. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R115

Managed-change provenance. Bind review and publication observations only through the saved repository, change request, request lineage and verified host facts. A similar PR body marker or Issue number without the managed binding cannot retroactively adopt a foreign change.

Planned proof for AC1: Supply a foreign PR carrying copied provenance text and reject adoption; fail a binding read and retain an explicit unbound observation without repair dispatch. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R118

Unavailable blockers stay unresolved. Required dependency and blocker evidence must be well-formed, allowed by the saved scope and complete enough for its predicate. Corrupt attempt state and failed scans cannot clear blockers or erase their existing recovery accounting.

Planned proof for AC1: Use an out-of-scope dependency, malformed reference and failed collection in turn; record distinct reasons, keep the gate closed and dispatch no replacement. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R123

Verified publishing actor. Before a host write, resolve and validate the saved publishing actor and required capability; an identity hint or app slug alone is insufficient. User-token support remains valid under the approved actor model, so no source-specific App-only requirement is imported.

Planned proof for AC1: Provide absent, mismatched and verified actor evidence for the same intent and permit only the matching case; change current credentials and preserve the intent's actor identity. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R124

Exact retained finding source. Human questions and judgments derive only from the current named request and complete trusted snapshot. Workspace-local lookalikes, escaped paths, oversized unreadable bodies or an older populated report cannot replace the newest missing or empty result.

Planned proof for AC1: Place findings in a foreign checkout and point outside the evidence root, then reject both; make the newest snapshot unavailable and show the older snapshot is historical only. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R128

No-change and refusal outcomes. Nothing-to-fix is a claim about an exact artifact and requirement set, not an exception to check, review or stop proof. Refused reports retain their typed human or repair reason and cannot become clean completion through a textual marker.

Planned proof for AC1: Return no-change with an empty check list despite a nonempty requirement set and deny completion; return refused with exit zero and require the recorded handoff. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R129

Cumulative correction lineage. Count every durable coding or review correction request against the applicable Work bounds, including interrupted and successor-Execution attempts. Replacing display records or moving a request out of a current view cannot hide its cost or restore an allowance.

Planned proof for AC1: Create correction requests across two Executions, interrupt one before report collection and reach the same cumulative cap after restart. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R158

Closed source and publication facts. A closed source Issue holds new dispatch and publication for human handling without automatic cancellation. Unknown source state cannot become permanent ineligibility, and only verified current publication facts may satisfy a publication predicate.

Planned proof for AC1: Close the source during a live Invocation, stop safely and release capacity while retaining WIP; make the source unreadable and assert neither cancellation nor success is inferred. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R162

Produced-head publication proof. A Worker fixed or complete report must be matched to actual host publication of its produced commit. Definitively unpushed change retains its artifact for repair; transient lookup failure is bounded recollection and eventual Attention, not publication success.

Planned proof for AC1: Compare exact produced and host commits, then test missing push and an unavailable comparison read; preserve the artifact and distinct invalid or unknown reasons in both failures. See the [governing contract](evidence-gates.md#incident-contracts-and-regression-scenarios).

## R169

Complete input and bounded assessment. Every required member or prerequisite must have a current valid result before one aggregate gate passes. Technical assessment failure follows bounded recovery, while an explicit denial or human decision cannot be retried as a transport fault; richer grouped intake remains deferred.

Planned proof for AC1: Omit one required prerequisite and reject aggregate success; produce a denial and assert no immediate retry, then test a retryable read failure under the saved limit. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R183

Typed bound repair records. Recovery evidence names the exact Work, request or action, closed failure class and source evidence version. Unreadable, wrong-kind or mismatched records cannot prove a block repaired or authorize a different recovery path.

Planned proof for AC1: Reuse W1's repair record for W2 and reject it; corrupt the class field and retain Attention with an invalid-evidence reason rather than treating the record as absent permission. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R186

Permission-policy provenance. Keep the effective session permission boundary and its provenance bound to the request, distinct from arbitrary inherited user configuration. A reported allowlist is not proof of enforcement, and a Worker cannot override saved denied capabilities through its own config.

Planned proof for AC1: Inject an inherited broad grant and show it cannot enlarge the accepted request; omit enforcement evidence and refuse dispatch even when the generated prompt claims restrictions. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R187

Scope-bound operation grants. Any local write or host-push authority is limited to validated managed paths and the saved branch target. Escaped paths, ambiguous tokens, symlinks outside the managed scope and malformed branch identities cannot be converted into grants.

Planned proof for AC1: Supply a path containing traversal, an escaping symlink and an invalid branch target; reject each before effects, and permit only the exact saved target with valid evidence. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R191

Untrusted report content. Validate the declared structured machine contract and request identity rather than scanning free text for a reassuring status or PR URL. Quoted reviewer content and a foreign process's report cannot override current validated facts or stop uncertainty.

Planned proof for AC1: Embed a success marker in quoted review text followed by a valid failed report and retain failure; supply a foreign PR URL and show no adoption or success. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R193

Exact actionable set and bounded loop. Correction works from the current configured source, exact occurrence set and accepted decisions. Repeated equivalent failures, pending checks and permission-repair waits have separate reasons and limits; none silently marks findings processed or restores spent attempts.

Planned proof for AC1: Repeat the same unresolved set across two correction failures and obtain Attention; hold pending CI without dispatching a fix, then require changed permission evidence before repair resume. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R239

Credential-scoped Worker environment. Pass only declared Worker-needed credential references through the trusted launcher; orchestration and delivery secrets are not Worker evidence or ambient authority. A report cannot request additional inherited secret names, and diagnostics must not echo rejected values.

Planned proof for AC1: Seed orchestration secrets and one approved model credential, inspect the child environment and permit only the declared credential; reject an undeclared reference without leaking its value. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R240

Scoped agent and diagnostic results. An agent recommendation or diagnostic result is bounded untrusted input under the original task and closed schema. Unknown verdicts, extra-scope members and diagnostic timeouts cannot change the selected Work, approval state or failure classification by guesswork.

Planned proof for AC1: Return an extra Issue and unknown verdict and refuse them; time out a diagnostic read and preserve the original block with explicit unavailable evidence. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R245

Read-only preflight significance. Capability preflight distinguishes a validated success, explicit denial and unknown transport result. Unknown required capability holds dispatch without inventing a permission denial or healthy state; preflight performs no host mutation.

Planned proof for AC1: Inject credential denial, DNS failure, timeout and a valid result for one required probe; assert distinct repair or unknown reasons and no launch except after valid evidence. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R246

Repair-plan evidence binding. If a correction plan is used, bind it to exact head, occurrence-set digest, accepted decisions and producer provenance. A changed premise invalidates its use, malformed planning output authorizes no fix, and planning cannot create an extra uncounted correction allowance.

Planned proof for AC1: Reuse a plan under identical binding, reject it after head or set changes, and crash planning output before validation with zero fix dispatch; keep consumed Work usage. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R247

Configured review source and actor. Admit required findings only from the configured request or eligible normalized source with verified scoped identity and exact target. Markers, trusted-looking account names and review completion on a different head cannot satisfy provenance or the enabled review lane.

Planned proof for AC1: Forge an own-looking marker from a foreign actor, change its actor ID and report another head; reject all three while a complete matching source passes. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R248

Honest dispatch accounting. Charge the durable request according to the target accounting contract, including definitely-not-started Invocations; do not import source-specific free-pass formulas or refund heuristics. Missing cost is unknown rather than zero, and interruption retains both request identity and consumed usage.

Planned proof for AC1: Refuse process start after request persistence and retain one counted Invocation; interrupt a reported-cost collection and show unknown cost without refunding its dispatch. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R249

Pushed fixes remain incomplete. A successful fix push records only that effect and new current target. The Work remains pending while new-head verification, review, replies or other required effects are owed; the old head's clean result cannot settle the new change.

Planned proof for AC1: Push a correction then interrupt before re-review, assert pending with the pushed head and owed requirements, and resume without repushing the already-applied effect. See the [governing contract](evidence-gates.md#incident-contracts-and-regression-scenarios).

## R250

Human findings and explicit disposition. Human-origin findings admitted by the configured source use the same exact-set decision and bounded fix rules. A proposed answer or reasonless dismissal is not operator acceptance, and unresolved required replies or threads retain their separate effects.

Planned proof for AC1: Propose an answer to one human finding and omit another, then reject completion; publish a verified fix reply but leave resolution unknown and retain partial status. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R251

Durable usage and approved extensions. Keep cumulative Work usage and any measured cost with exact request identities, and record unknown measurements explicitly. An operator may extend a configured bound through the approved versioned policy-change path but cannot reset consumed counters or bypass unknown effects.

Planned proof for AC1: Spend a correction, restart under a new Execution and retain usage; request an extension with stale WorkVersion and reject it; accept a valid approved extension without zeroing counters. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R252

Ready effect without automated merge barrier. The reference workflow publishes a new PR ready after pre-publication gates, independently of optional post-publication review. An explicitly required ready action needs its own exact-target receipt, but this mapping does not reinstate draft holds, automatic Request changes or repository-rule discovery.

Planned proof for AC1: Enable code review and create a ready PR before that lane completes; lose an explicitly requested ready response and reconcile only that action without repeated state toggles. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R257

Ordered publication prerequisites. Publication requires exact binding, retained artifact, verified pre-publication requirements and a saved host intent. Failed prerequisite checks produce typed refusals without host writes; an existing successful publication receipt remains history rather than being erased by a later failure.

Planned proof for AC1: Alter artifact digest or verification identity and assert zero push/create calls; complete a valid push then fail create and retain the applied push receipt. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R259

Named CI stays required. A configured CI identity requires the latest applicable successful attempt on the exact current target. Registration timeout, an absent check, another producer or a previous head's success cannot satisfy it; an empty configured list remains a distinct policy fact.

Planned proof for AC1: Keep a named check absent beyond its observation window and obtain Attention rather than success; advance the head and require a new applicable result while CI-empty Work adds no dependency. See the [governing contract](evidence-gates.md#ci-identity-and-requirement-snapshot).

## R260

Classification before fix authority. Classify review records as actionable, non-actionable, human-decision or unavailable under the saved source and policy. A severity recommendation cannot promote foreign or untrusted findings into fix authority or accept a risky judgment on the operator's behalf.

Planned proof for AC1: Submit a summary-only body, a security decision and a foreign marker, then retain distinct classifications and dispatch no unauthorized fix; allow only a valid current Fix decision. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R261

Answerable immutable judgment episodes. An operator decision binds to an immutable round, exact target, WorkVersion and full occurrence set. Equal Command replay is idempotent, stale or competing decision bundles are rejected, and Wait persists a human hold without resetting usage.

Planned proof for AC1: Replay one valid decision bundle with no duplicate effect; move head or round before a second bundle and reject it; keep a sibling fix from silently clearing Wait. See the [governing contract](evidence-gates.md#same-head-growth-and-exact-set-guards).

## R263

Attributed complete event evidence. Worker observations must identify their request and session and satisfy the declared completion schema. A broken stream, foreign event, untagged idle status or failed cleanup cannot fabricate a terminal result; retain evidence availability and cleanup outcome separately.

Planned proof for AC1: Mix two sessions on one stream and accept only the bound events; cut the stream before its terminal report and preserve unknown completion despite a later idle observation. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R264

Preserved remediation contract. A corrective request inherits the original approved requirements and explicit failing-evidence selection together with its actual predecessor artifact. It cannot replace failing results with a passing fallback or silently narrow the acceptance scope.

Planned proof for AC1: Build a correction from a selected failed verification and assert original invariants and checks remain; offer a passing unrelated result and reject substitution. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R265

Non-actionable review records. Normalize incoming review feedback before deriving a required occurrence or fix request. Clean summaries, badges and unsupported verdicts remain distinct non-actionable or invalid records; unfamiliar text cannot be guessed into a Fix decision.

Planned proof for AC1: Feed a clean summary, status badge and unknown verdict and assert zero fix dispatch with distinct recorded reasons, while a valid configured-source occurrence is retained. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R266

Historical dataset boundary. Historical review capture remains untrusted evidence with immutable input identity, declared coverage and unresolved-reference reasons. Dataset normalization and sanitized exports do not constitute current live review or automatic finding dispositions; a standalone dataset product is not imported.

Planned proof for AC1: Normalize identical captured bytes deterministically, retain an unresolved reference, and try to submit the export as a current live review; deny that gate substitution. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R267

Snapshot-bound verdict. Bind each verdict to immutable snapshot bytes, source completeness and exact head/base identities. A partial snapshot can support recorded partial observations but cannot seal a complete clean review or satisfy an enabled lane.

Planned proof for AC1: Omit required CI evidence and record partial availability; submit a verdict against altered snapshot bytes and reject it without a new current judgment. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R268

Human adjudication separation. Keep human-authored adjudication provenance distinct from model recommendations and unreviewed datasets. An evaluation label cannot grant live operator authority, and a model-produced label cannot claim independent human review.

Planned proof for AC1: Provide a model label with a human-reviewed flag and reject it; preserve valid human provenance as historical evaluation evidence without accepting a current finding judgment. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R270

Honest comparison results. Any comparison report binds to the exact evaluated verdicts, adjudications and coverage, and excludes unavailable or incomparable inputs from claims of measured success. Comparison metrics are not a replacement for current Work evidence or a required v0.1 evaluation service.

Planned proof for AC1: Supply an inconclusive verdict and wrong snapshot binding, then report explicit missing or invalid comparison input rather than a zero-error score or live success. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R271

Bounded source capture. Collect source content within declared limits and record every omitted, truncated or unverifiable part with a reason. Stat-only or incomplete capture cannot establish full review coverage, and collection never mutates the source host.

Planned proof for AC1: Exceed file and byte bounds and preserve named omissions; fail head verification and deny clean review while retaining partial capture metadata. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R272

Archive evidence and comparability. Archive records distinguish missing result bodies, incomplete pagination and simulated provenance from validated same-target evidence. Equal display labels or head strings alone cannot establish comparable reviewer coverage, and archive inventory cannot certify live behavior.

Planned proof for AC1: Load an archive with missing result and incomplete pages, then report unavailable or incomparable coverage; mark fixture provenance and reject its use as live acceptance. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R273

Advisory-source quarantine. The configured required review source determines actionable evidence; unsolicited or shadow sources remain advisory. Changing discovery or display mode cannot make those records accepted Fix decisions or silently weaken an existing Work's review requirement.

Planned proof for AC1: Present an advisory finding from a familiar actor and dispatch nothing; require an authorized policy change and a new applicable assessment before changing the required source. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R274

Undecided evaluation remains undecided. Trial and evaluation records retain deterministic input identity and explicit unset decisions. Missing thresholds or unpaired findings cannot produce a passed acceptance claim, and per-head experiments remain outside the live completion proof.

Planned proof for AC1: Replay identical trial inputs and obtain identical evidence identity; omit a criterion decision and assert undecided rather than passed or zero misses. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R277

Untrusted text in host projections. Treat finding text as data and neutralize embedded host mentions or control markers before projection. Generate the authentic provenance marker from the saved action identity, retain the authorized payload digest and never parse embedded user text as publication authority.

Planned proof for AC1: Project a finding containing a forged marker, closing delimiter and mention; verify only the genuine saved marker can reconcile the post and the text creates no new action. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R278

Target-bound feedback publication. When a task requests feedback publication, its saved intent binds to the exact change, target revision and validated position. Unavailable or invalid positions may be reported honestly as unanchored only under the accepted payload; they cannot silently retarget to a new head or imply a native merge gate.

Planned proof for AC1: Move the head between capture and publication and invalidate the old authorization; reject an invalid inline position without duplicating publication or claiming it landed. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R279

Check projections are effects. An optional host check projection reports the persisted assessment for its exact head and attempt; it is not independent evidence that the underlying gate passed. Technical publication failure, incomplete coverage and valid negative verdicts retain different receipts and reasons; no native check-based merge barrier is required.

Planned proof for AC1: Publish a valid negative projection, then lose its response and reconcile the same action; move the target before posting and prohibit a stale success projection. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R280

Owned feedback identity and reconciliation. Deduplicate a requested feedback effect through saved action identity and complete own-authored host evidence, not text similarity. A forged foreign-authored marker cannot satisfy, edit or delete the original action; changed content needs a fresh authorized payload and guards.

Planned proof for AC1: Post once, lose the response and reconcile without duplication; inject the same marker on a foreign comment and keep the intended action unknown without editing the foreign surface. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R281

Discovery and reconciliation evidence. Record review discovery completeness separately from publication or thread-reconciliation completeness. A review task presented as independent discovery excludes prior verdicts from its discovery input; reconciliation consumes them only after the new verdict is frozen. A complete discovery verdict does not prove all effects landed, while an incomplete discovery cannot be made clean by successful effect reconciliation; no standalone comparative-review pipeline is introduced.

Planned proof for AC1: Complete discovery and fail a thread read, preserving review evidence and unknown effect separately; omit discovery evidence and refuse complete review despite a successful host read. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R282

Actual producer provenance. Record requested and adapter-reported model identities, adapter, prompt version and effective policy fingerprint separately when those facts are relevant to the configured review task. Unreported effective identity is unknown, and a requested model must not be presented as an observed one.

Planned proof for AC1: Have an adapter report a substituted model and retain both values; report none and retain an explicit unresolved provenance reason even on an early refusal. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R283

Intent before feedback writes. Persist an immutable reviewed snapshot and each feedback intent before host mutation, and reconcile ambiguous responses against exact own-authored postconditions. Dry-run stages cannot claim publication, and a partial local commit cannot establish a completed review or duplicate host effects.

Planned proof for AC1: Crash after host posting but before receipt save and adopt one exact own-authored result; repeat with a copied foreign marker and keep the original effect unresolved. See the [governing contract](evidence-gates.md#structural-storage-and-transactions).

## R284

Positive disposition evidence. A finding becomes fixed only through a verified correction/publication/reassessment chain or receives an explicit authorized Accept or Dismiss judgment. A partial or failed later review cannot erase prior occurrences, and complete nonappearance alone does not prove a particular repair.

Planned proof for AC1: Omit an old finding from partial and complete reviews and retain the respective availability or nonappearance fact without inventing a fix; submit a valid explicit dismissal with rationale. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R285

Evidence limits in reports. Distinguish implementation failure, missing input, unavailable provider, incomparable baseline and unmeasured usage in every reported result. Mock or fixture evidence cannot attest real-model, live-host or E2E criteria, and unknown values are not invented as zeros.

Planned proof for AC1: Run a fixture without a provider or cost measurement and report both as unknown; try using a mocked pass as live acceptance and reject it. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R291

Bounded untrusted prompt material. Bound attacker-influenced findings, diffs and prior reports before including them in a task, and frame them as untrusted evidence rather than instructions. Record truncation and coverage consequences; an embedded command cannot approve a finding or widen the request.

Planned proof for AC1: Include a large finding containing an approve instruction and verify bounded untrusted framing; truncate required evidence and mark the corresponding assessment incomplete. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R292

Conflict detection with human handling. Identify a conflict by repository, change request, head and target revision, and hold dependent publication for human handling under #57's approved scope. Automatic conflict-repair workers remain deferred; repeated observation cannot mint paid repairs or reuse assessment from an old head/base pair.

Planned proof for AC1: Observe the same conflict twice and create one current hold with no repair Invocation; advance only the base and record a new episode requiring fresh evidence. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R293

Non-fast-forward publication refusal. A rejected guarded push retains the old artifact and definite refusal reason, then requires current host evidence and fresh guards. Do not import automatic patch replay onto a moved head or reuse old verification for the rebased change; permission denial follows environment repair.

Planned proof for AC1: Move the branch before a guarded push and refuse the update without dispatching an unapproved replay; inject permission denial and retain its distinct repair prerequisite. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R294

Named check failures and diagnostics. A configured check's failed or error result remains valid negative evidence for its exact producer, head and attempt. Bounded sanitized logs support diagnosis, but missing logs do not become a clean check, and check failure cannot auto-create arbitrary required review sources.

Planned proof for AC1: Return failure with two diagnostic excerpts and no available full log, then retain the failed check and missing-log reason; present another producer's success and keep the requirement failed. See the [governing contract](evidence-gates.md#ci-identity-and-requirement-snapshot).

## R295

Bound correction strategy decisions. Any saved strategy recommendation binds to the current head, exact findings, accepted decisions and retained supporting context before a correction request is considered. Repeated invalid recommendations or changed premises require bounded Attention; no additional planner port or autonomous operator judgment is introduced.

Planned proof for AC1: Replay a strategy on identical input without new dispatch, change one occurrence and reject its reuse, and return malformed strategy output with no authorized correction. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R298

Own-state recovery without destructive guesses. A recovery decision may reuse or repair only state whose exact Work, request, artifact and action provenance is verified. Unknown dirty files, foreign commits or moved branch tips prohibit destructive cleanup; source-specific automatic revert/delete machinery is not imported.

Planned proof for AC1: Interrupt publication recovery, then supply a foreign dirty change and refuse cleanup while retaining evidence; reuse the exact unchanged retained artifact without rewriting foreign state. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R299

Reproducible artifact identity. If an adapter reconstructs a retained change, verify its exact expected commit and content digest before accepting it as the artifact. Config, hooks, path traversal or a similarly named recipe cannot silently substitute different bytes; no recipe materializer is required by v0.1.

Planned proof for AC1: Reconstruct the same artifact twice and verify identical identity; alter one file or introduce an escaping path and reject reconstruction before it can satisfy verification. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R300

Durable Work attempt accounting. Count automatic dispatch and recovery requests under one Work lineage across specification revisions and successor Executions. A deterministic refusal is not permission to repurchase the same attempt, and changed input still needs renewed approval without resetting usage.

Planned proof for AC1: Reapprove a source revision after a failed dispatch, restart and retain consumed counters; submit the identical refused conditions again and obtain no new automatic Invocation. See the [governing contract](evidence-gates.md#work-wide-recovery-accounting).

## R302

Changed specification and evidence binding. A changed accepted decision produces a new approved specification or invariant identity rather than editing old Invocation input. New assessment and dispatch bind to that exact revision; failed persistence prevents new launch and removing a decision cannot leave unrecorded stale constraints.

Planned proof for AC1: Change an approved decision and reject reuse of the old request; fail saving the new revision and assert zero dispatch while the old evidence remains intact. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R304

Strict checker boundary. Parse only the declared bounded schema for a configured checker and reject malformed, contradictory, duplicate or incomplete coverage. Prompt echoes and delimiter-looking text are untrusted payload, and truncating required decisions cannot convert invalid output into a clean result.

Planned proof for AC1: Return a prompt echo, contradictory clean result with violations and an over-limit decision set; retain invalidity reasons and authorize no publication. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R306

Fixture and live proof separation. An offline harness records the exact task, adapter, simulated mode, expected result and reproduction command. Optional external findings and incomparable baselines cannot silently alter declared gate semantics, and a fixture pass does not prove a live lane or real model succeeded.

Planned proof for AC1: Run a fixture with valid mechanical evidence and unrelated advisory findings, then preserve their separate roles; mismatch a declared expectation and report failure without any live completion claim. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R309

Provider failure provenance. Classify provider refusal, cancellation, setup failure and unknown result separately, preserving each dispatched attempt's usage and unknown measurements. No implicit fallback candidate loop is introduced; any configured adapter fallback must remain within the immutable task and applicable Work bounds.

Planned proof for AC1: Record a refused attempt with unknown usage and prohibit a fabricated total; cancel an attempt and show it cannot silently dispatch a replacement or reset counters. See the [governing contract](evidence-gates.md#failure-classification-and-recovery-decisions).

## R310

Reusable comparison evidence. Reuse a prior evaluation result only when task bytes, referenced evidence bytes, mode and comparison rules still bind exactly. Changed baseline content behind an unchanged reference invalidates comparison, and evaluation reuse never supplies current live completion evidence.

Planned proof for AC1: Edit baseline bytes without changing their path and require reevaluation; change comparison rules and preserve the old result as historical instead of presenting it as current. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R311

Cancellation and bounded safety reads. Cancellation prevents dependent new execution but must still permit separately bounded stop, evidence preservation and original-action reconciliation. The cancelled worker task's timeout cannot itself cancel the only read needed to establish safety or turn failed cleanup into stop proof.

Planned proof for AC1: Cancel normal transport while a write response is lost, then complete a bounded independent reconciliation read; fail that read and retain unknown ownership rather than releasing resources. See the [governing contract](evidence-gates.md#outcome-counters-and-repair).

## R312

Sanitized payload identity. Keep authorized action identity and wire-payload digest distinct from sanitized diagnostic-body digest. Secret-bearing required bodies must use the trusted content-retention boundary or explicit unavailable handling; a redacted body alone cannot prove exact remote payload or publication.

Planned proof for AC1: Redact credential-shaped text in a projected payload while preserving its exact wire digest, then compare authoritative remote content; a saved payload with no receipt remains unpublished. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R313

Current source authority. Install current assessment pointers only through the configured source's validated revision and collection contract. Replayed or out-of-order observations retain history, cannot restore a superseded generation and cannot resolve or reopen findings merely by local arrival order.

Planned proof for AC1: Commit a newer source snapshot, then deliver an older revision and duplicate collection; preserve history but keep current judgments and authorizations unchanged. See the [governing contract](evidence-gates.md#same-head-growth-and-exact-set-guards).

## R314

Transactional and guarded effect queue. Save effect intents with the decision that authorized them and recheck current generation and ownership before applying an uncommitted dependent step. Superseded intents cannot publish withdrawn feedback late; already-applied effects retain their receipts and unknown effects retain original reconciliation identity.

Planned proof for AC1: Stage feedback, install a superseding assessment before execution and refuse the stale write; crash after a prior effect lands and reconcile it without resending. See the [governing contract](evidence-gates.md#stage-effects-and-reconciliation).

## R318

Validated citation positions. Resolve evidence citations against the exact retained checkout and one declared position grammar, refusing traversal, escaping symlinks, inverted ranges and unavailable lines. Position repair may use only already-cited evidence; inventing a path or retargeting a different head is prohibited.

Planned proof for AC1: Supply an escaping path, an out-of-range line and a citation without a path and reject them; canonicalize an equivalent valid range without changing its target bytes. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R319

Canonical immutable evidence. Persist versioned canonical bytes and digests atomically with their metadata references. Partial writes, conflicting bytes under one identity and later corruption are explicit invalid evidence, not absent permission to start over.

Planned proof for AC1: Interrupt content persistence before completion and leave no accepted partial record; reorder canonical set input without changing its digest, then corrupt stored bytes and reject reuse. See the [governing contract](evidence-gates.md#structural-storage-and-transactions).

## R320

Bounded process evidence. Process supervision must prove the whole recorded writer tree stopped, and captured output has declared byte bounds with truncation metadata. Correct text decoding cannot substitute for a missing terminal record, and a spawn error retains its typed start result.

Planned proof for AC1: Spawn a grandchild then stop the Invocation and inspect all descendants; exceed output bounds and split multibyte text across chunks while preserving explicit truncation and terminal availability. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R321

Complete stream decoding. A transport decodes the declared event schema across arbitrary byte boundaries and flushes its final complete event before deciding report availability. Truncated or malformed final data remains incomplete rather than becoming idle, clean or complete-empty evidence.

Planned proof for AC1: Replay one stream split at every byte boundary and get the same terminal report, then truncate its last event and assert an incomplete-report result. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R322

Exact change admission. When a change request exists, verify its repository, head/base, saved policy and live open state against the accepted binding before dependent correction or publication. Pre-publication correction instead binds its exact predecessor artifact and failed evidence without fabricating a PR identity. Foreign ledgers, unsupported fork targets and closed or merged PRs cannot supply continuation authority; unknown host state remains a hold.

Planned proof for AC1: Provide a foreign repository record, fork target and merged PR and assert zero mutation; make live state unreadable and retain unknown rather than treating it as open. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R323

Proof of claimed fixes. A Worker-declared fix requires the exact predecessor finding and retained changed artifact, independently verified publication and applicable reassessment. Touching the named path is supporting evidence only; a mismatched or unverifiable path cannot justify a fixed reply or silent closure.

Planned proof for AC1: Test changes that miss, rename or merely copy the finding path, then require actual reassessment; even a matching-path change without push evidence cannot produce a fixed disposition. See the [governing contract](evidence-gates.md#immutable-review-snapshots-and-finding-identity).

## R324

Saved review-mode policy. Resolve review-source and publication-mode choices into the accepted effective policy rather than mutable per-run ambient overrides. An explicit authorized policy change preserves history and usage; changing display mode cannot admit advisory findings or waive a required source.

Planned proof for AC1: Change current mode while a Work waits and keep its original source contract; approve a valid policy update and require fresh bound evidence without copying old dispositions. See the [governing contract](evidence-gates.md#owner-approved-lanes-and-caller-flow).

## R329

Secret-safe diagnostic projections. Sanitize credential-shaped values in job errors, per-attempt reports and exposed review evidence before projection. Do not silently retain secret-bearing general-purpose logs as canonical truth; exact required sensitive content follows trusted retention and action digests, while redacted text never proves a host effect.

Planned proof for AC1: Inject a secret into an error and finding body, inspect every exposed artifact and output, and verify the original action still reconciles by its saved identity without displaying the secret. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R331

Deferred annotation tooling and provenance. Dedicated blinded annotation tooling remains an explicitly deferred mechanism. If its evidence is consumed later, retain exact finding/snapshot binding and distinguish human labels from unreviewed model material; annotation terms or missing identity cannot establish independent review.

Planned proof for AC1: Provide a brief naming another occurrence and reject its evidence link; preserve a valid historical annotation without using it as a current live operator decision. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).

## R332

Immutable prompt provenance. A configured checker or reviewer task records the exact supported prompt version and content identity used for initial and format-repair attempts. Unknown versions refuse dispatch, and a repair cannot silently substitute a newer prompt while claiming the old provenance.

Planned proof for AC1: Run a malformed first result through format repair and retain the same prompt version; request an unsupported version and assert zero dispatch, then detect altered bytes under an existing version. See the [governing contract](evidence-gates.md#evidence-schema-and-validity).
