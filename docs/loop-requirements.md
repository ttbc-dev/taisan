# Execution-loop requirement mappings

Normative mapping of every requirement assigned to #55 by the extraction inventory onto the [execution loop, CLI contract, and skill workflow](execution-loop.md) clauses and planned verification suite. Each `## R###` section names the #55-owned obligation — the part the loop, contract, recovery scheduler, or skill surface must satisfy — not the requirement's entire source mechanism. Obligations owned by [installation config](installation-config.md), [evidence gates](evidence-gates.md), [worker protocol](worker-protocol.md), [identity/storage](identity-storage.md), or [retention](retention-policy.md) are cited, not duplicated.

Many source requirements describe ops-repo machinery — a scheduler service, card board, console/projection binaries, notice ledgers, local-agent session producers — with no counterpart in the target installation. Per owner decision every such requirement is re-modelled: the mechanism is omitted and its retained safety obligation maps onto the serial loop, durable `recovery_intents`, the closed CLI contract, fail-closed reconciliation, or Query-only Attention. Cross-task doctor diagnosis, TUI delivery, and MCP are deferred under the [post-v0.1 milestones](post-v0.1-milestones.md); those deferrals are cited where they cover a mechanism. No new deferrals were created.

## Reading and mapping rules

- **Clause**: the governing normative section(s).
- **Suite**: the LP-xx planned verification case(s) in [execution-loop.md](execution-loop.md#planned-verification-suite).
- **Planned proof**: the fault-injection or contract scenario satisfying the #55 acceptance row (mostly AC5's failure matrix).

## R08

Unknown observations and repair prerequisites — loop slice: a failed source read, missing scan, or unreadable record is `unknown`, never free capacity, absent findings, or repair evidence.

Clause: [failure matrix](execution-loop.md#failure-scenario-matrix) F10/F14; [evidence classes](execution-loop.md#evidence-classes). Suite: LP-07, LP-22.

Planned proof: kill the source mid-refresh — Work holds with `unavailable`, capacity unchanged, no dispatch; resume demands fresh validated evidence.

## R10

Atomic dispatch evidence — the `queued→ready` transition, capacity reservation, InvocationRequest persist, and budget charge are one store transaction; a persistence error authorizes no launch.

Clause: [tick phases](execution-loop.md#tick-phases-in-order) phase 2; F1. Suite: LP-10.

Planned proof: crash the dispatch commit — no request, no writer; a partial write is impossible by transaction atomicity.

## R11

Lost launch acknowledgement — an absent start receipt records `unknown` under the original request; new keys and successor Executions cannot bypass the uncertainty.

Clause: F2; [phases](execution-loop.md#tick-phases-in-order) 2–3. Suite: LP-11.

Planned proof: kill the loop between request persist and spawn, then between spawn and receipt — restart reconciles the recorded request, never mints a second writer.

## R13

Immutable bounded Invocation — the request fixes task/base/requirements/limits before dispatch; per-Execution and Work-wide limits compose.

Clause: [phases](execution-loop.md#tick-phases-in-order) 2–3; [bounded recovery](execution-loop.md#bounded-automatic-recovery). Suite: LP-10, LP-23. Governing semantics cited from [worker protocol](worker-protocol.md#schemas).

Planned proof: mutate the stored request post-dispatch — the running Invocation keeps its immutable input; exhausted bounds block further starts.

## R15

Stop proof and uncertainty — stop intent persists before signalling; every recorded descendant carries reuse-resistant identity; ambiguous inspection keeps ownership.

Clause: F3/F4; [timeout recovery](execution-loop.md#timeout-recovery-sequence). Suite: LP-12, LP-13.

Planned proof: zombie leader, escaped child, recycled PID — no replacement writer and no slot release until stop is proven.

## R18

Typed failure classification — structured request/evidence/stop facts classify independently of process exit status; the classifier key feeds equivalent-failure suppression.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); [recovery policy](recovery-policy.md#failure-and-progress-classification). Suite: LP-26.

Planned proof: exit-0 crash, exit-1 refusal, timed-out stop — each classifies by evidence, not status code.

## R24

Bounded reasons and retained diagnostics — closed failure categories with bounded sanitized display text; raw diagnostics retained by reference.

Clause: [envelope](execution-loop.md#envelope) `reason`; [sanitization suite](execution-loop.md#planned-verification-suite) LP-28. Suite: LP-02, LP-28.

Planned proof: inject a 10 KiB provider stack trace — the envelope carries a bounded `reason.code` and evidence ref, never the raw text.

## R25

Exact judgment target — a judgment binds to current WorkVersion, specification, policy, head, and the complete occurrence-set; same-head growth invalidates the draft.

Clause: F8; [command surface](execution-loop.md#command-surface) `judge-findings`. Suite: LP-16. Semantics cited from [evidence gates](evidence-gates.md).

Planned proof: add a finding on the same head between inspect and submit — acceptance rejects with coverage conflict.

## R26

Bounded convergence and progress — policy-bounded correction loops; semantic progress rearms the no-progress signal but never restores consumed budgets.

Clause: [progress detection](execution-loop.md#progress-detection); F11/F12. Suite: LP-19, LP-24, LP-26.

Planned proof: material progress resets the warning clock while consumed attempts and elapsed time stay charged.

## R27

Durable judgment history — immutable rounds/occurrences/decisions survive restart and successor Executions; history never rebinds to a different target.

Clause: F8; [identities](execution-loop.md#identities-and-preconditions-for-mutation). Suite: LP-16.

Planned proof: restart mid-judgment — recorded rounds are retained, and a different head cannot inherit prior decisions.

## R29

Unknown ledger/orphan findings never dispatch mutation — a parseable-empty or corrupt record surface is `unknown`; a finding bound to a foreign PR cannot authorize an effect.

Clause: F10; [evidence classes](execution-loop.md#evidence-classes). Suite: LP-07, LP-22.

Planned proof: corrupt the judgment record and attach an orphan finding — Query surfaces `unknown`/`unavailable`, and no outbox effect is authorized.

## R30

Read-only reconciliation — reconciliation inspects the original action by exact identity/payload/actor/postcondition; ambiguous matches stay `unknown`.

Clause: F6/F7; [phases](execution-loop.md#tick-phases-in-order) 4. Suite: LP-15.

Planned proof: lose the response after a push-write — reconcile read-back adopts only an exact match; ambiguity remains `unknown` and unwritten.

## R31

Replay without new authority — stable Command/Action identities return saved decisions; a replay cannot retire newer requests or mint effects.

Clause: [envelope](execution-loop.md#envelope) replay semantics; [preconditions](execution-loop.md#identities-and-preconditions-for-mutation). Suite: LP-06.

Planned proof: replay a terminal Command after newer work — the recorded decision returns unchanged, no new mutation.

## R32

Recorded bounded recovery — recovery class, evidence, attempt charge, and deadline are recorded before any retry; missing vs corrupt artifacts classify differently.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F5. Suite: LP-25, LP-14.

Planned proof: crash between decision and dispatch — the persisted intent charges and fires exactly once; a corrupt artifact never masquerades as missing.

## R33

Changed-evidence resume — a repair decision names the failed prerequisite and the evidence that must change; resume revalidates fresh evidence, version, and binding.

Clause: [recovery scheduling](execution-loop.md#tick-phases-in-order) phase 6; [resume semantics](execution-loop.md#bounded-automatic-recovery). Suite: LP-25.

Planned proof: hold on permission denial; unchanged evidence cannot resume, a fixed credential plus revalidation can.

## R34

Foreign or unknown records never reclaim or release — fieldless, foreign, or unreadable observations never release a fence or free a slot.

Clause: F3/F4; [phases](execution-loop.md#tick-phases-in-order) 3. Suite: LP-12, LP-13. Retention slice cited from [retention requirements](retention-requirements.md#r34).

Planned proof: feed observations from a foreign installation — ownership holds, no release, no settlement.

## R35

Actionable Attention evidence — Attention derives from current unresolved prerequisites with version, reason, and required action; acknowledgment changes presentation only.

Clause: [Attention delivery](execution-loop.md#owner-decisions); [phases](execution-loop.md#tick-phases-in-order) 8. Suite: LP-22.

Planned proof: acknowledge an Attention item — it hides but never releases guards; unacknowledge/re-derive restores visibility.

## R36

Visible unknown sources — missing, partial, invalid, unavailable, and complete-empty sources stay distinct with retained reasons.

Clause: F10; [queries](execution-loop.md#envelope). Suite: LP-07.

Planned proof: one malformed source record cannot hide unresolved Work — the Query still exposes the exact class and reason.

## R37

Effective configuration evidence — assessment refers to the saved effective policy/actor/capability identity; a health-check claim needs exact validated configuration.

Clause: [dispatch revalidation](execution-loop.md#tick-phases-in-order) phase 2; [installation config](installation-config.md). Suite: LP-04.

Planned proof: change config after admission — the accepted Work keeps its saved effective policy; claims without validated config fail.

## R39

Per-Work material-progress observation — a durable progress clock records accepted artifacts, newly-valid checks, and resolved unknowns; it rearms the warning threshold and nothing else.

Clause: [progress detection](execution-loop.md#progress-detection). Suite: LP-19, LP-24.

Planned proof: log volume and a bare commit do not count as progress; an accepted artifact does.

## R40

Durable bounded automatic recovery across Executions — stop intent, proven stop, effect reconciliation, and budgeted requeue survive restart and Execution succession.

Clause: [timeout recovery](execution-loop.md#timeout-recovery-sequence). Suite: LP-23, LP-27.

Planned proof: crash at each sequence step — restart continues from the recorded step, budgets preserved across successor Executions.

## R43

Versioned retirement of pending recovery intents — terminal evidence durably and idempotently retires a pending intent; restart cannot resurrect obsolete work or reset bounds.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F13. Suite: LP-21, LP-25.

Planned proof: merge the PR between scheduling and firing — the intent retires with its evidence generation and never fires after restart.

## R47

Lane-doctor recovery skill implicit invocation — the doctor mechanism is deferred (cross-task diagnosis); the retained obligation is that recovery actions are invocable through the same agent-facing CLI surface with no separate authority.

Clause: [skill workflow](execution-loop.md#skill-workflow); [post-v0.1 milestones](post-v0.1-milestones.md). Suite: LP-08.

Planned proof: the skill surface exposes retry/resume/cancel/acknowledge only through typed Commands — no implicit doctor path exists.

## R51

Scheduler tick interval-deadline bounds — re-modelled onto the loop: a pass is deadline-bounded and defers remaining items to the next pass rather than running unbounded.

Clause: [loop model](execution-loop.md#loop-model) tick deadline. Suite: LP-04.

Planned proof: saturate a pass beyond 120 s — unfinished items report `tick.deadline` and process next pass; none are dropped.

## R52

Intake-mode rollback non-destructive — mechanism omitted (no intake modes); retained: a config or mode change never destroys in-flight Work, which keeps its saved effective policy.

Clause: [loop model](execution-loop.md#loop-model); [installation config](installation-config.md#rotation-renewal-and-binding-preservation). Suite: LP-04.

Planned proof: flip configuration mid-execution — in-flight Work proceeds under its recorded binding.

## R53

Session-ledger lifecycle events — mechanism omitted (no card ledger); retained: external agent session events ingest as deduplicated Observations and never complete Work.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1; [sanitization](execution-loop.md#planned-verification-suite) LP-28. Suite: LP-28.

Planned proof: replay a session-end event — idempotent ingest; a session event alone cannot settle an Execution.

## R82

Crash-cut re-entry — ordered reconciliation on restart adopts proven effects and never redispatches repair.

Clause: [loop model](execution-loop.md#loop-model) crash safety. Suite: LP-05.

Planned proof: crash mid-pass at each phase boundary — the next pass reconciles recorded identities; a proven applied effect is adopted, never repeated.

## R86

Permission-denial park keyed by binding fingerprint — re-modelled: a permission failure holds the Work under its recorded binding and lifts only on fresh validated evidence or explicit operator action.

Clause: [recovery policy](recovery-policy.md#failure-and-progress-classification) auth row; [changed-evidence resume](execution-loop.md#persisted-scheduling-and-retirement). Suite: LP-25.

Planned proof: deny the credential — the Work holds; unchanged evidence cannot resume, a rotated credential revalidated at dispatch can.

## R87

Terminal-block reenqueue requires recorded-identifier proof and a fresh intent — the original blocked work is untouched; requeue mints a new persisted intent.

Clause: F13; [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement). Suite: LP-21, LP-25.

Planned proof: requeue after terminal block — the new intent carries fresh identity and generation; the blocked record is unmodified.

## R89

Observation-exhaustion auto-unblock only for proven-live transient blocks — operator holds are never auto-released.

Clause: [phases](execution-loop.md#tick-phases-in-order) 6; F14. Suite: LP-25.

Planned proof: an operator-caused hold never auto-releases; a transient block auto-resumes only with proven evidence.

## R93

Baseline advances only after successful output — the ingest checkpoint advances only after the Observation commits; at-least-once redelivery deduplicates.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1. Suite: LP-04.

Planned proof: crash after reading a source but before commit — the next pass re-reads and deduplicates; no event is lost or double-applied.

## R95

Bounded subprocess termination — worker stop kills the complete recorded process group on deadline; semantics owned by [worker protocol](worker-protocol.md).

Clause: F3/F4; [worker protocol](worker-protocol.md#process-descendants-and-stop-proof). Suite: LP-12, LP-13.

Planned proof: an escaped descendant group is terminated by recorded identity; unrelated processes untouched.

## R96

Versioned envelope validation fails closed — unknown/newer `schema_version` is refused, never optimistically parsed.

Clause: [envelope](execution-loop.md#envelope). Suite: LP-01.

Planned proof: emit `schema_version: "2"` — the consumer refuses; unknown members do not corrupt known fields.

## R98

Serial worker with bounded pending queue — the loop is single-flight per installation and per-Work work items are bounded per tick.

Clause: [loop model](execution-loop.md#loop-model) run lock + tick bounds. Suite: LP-03, LP-04.

Planned proof: a second `run` refuses `lock.held`; overflow items defer deterministically, never queue unboundedly.

## R99

MCP lifecycle gating — the mechanism is deferred (MCP optional); retained obligation — session-scoped tool gating — has no v0.1 surface.

Clause: [post-v0.1 milestones](post-v0.1-milestones.md); [skill workflow](execution-loop.md#skill-workflow). Suite: none (deferred mechanism).

Planned proof: N/A in v0.1; the mapping records that no MCP surface exists to gate.

## R100

Stuck-work attention fires only on recorded timestamp-plus-budget proof — never on absent or ambiguous evidence.

Clause: [progress detection](execution-loop.md#progress-detection); F11/F14. Suite: LP-19, LP-22.

Planned proof: remove the progress timestamp — the signal degrades to `unknown`, never a false stuck alert and never silent health.

## R101

Attention ledger semantics — delivery is Query-only: an item persists unacked until acknowledged; per-item identity, version, reason, and required action are recorded.

Clause: [Attention delivery](execution-loop.md#owner-decisions); [phases](execution-loop.md#tick-phases-in-order) 8. Suite: LP-07, LP-22.

Planned proof: restart between derivation and query — items persist unacked; acknowledgment records a version, never erases the record.

## R104

Snapshot reuse inside freshness TTL — mechanism omitted (no snapshot cache); retained: Queries read the current store snapshot — no stale-cache answer exists to serve.

Clause: [envelope](execution-loop.md#envelope) queries. Suite: LP-07.

Planned proof: mutate state then query — the result reflects the current snapshot, never a cached view.

## R105

Typed action allowlist for mutations — every mutation is a typed Command carrying stable target identity, preconditions, and an idempotency key.

Clause: [command surface](execution-loop.md#command-surface); [preconditions](execution-loop.md#identities-and-preconditions-for-mutation). Suite: LP-01, LP-06.

Planned proof: attempt an untyped mutation — no path exists outside the typed Command surface.

## R107

Blocked re-dispatch requires passing preflight — a requeue consumes its intent only after full revalidation against current evidence and a fresh deterministic key.

Clause: [phases](execution-loop.md#tick-phases-in-order) 6; [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement). Suite: LP-25.

Planned proof: requeue with now-invalid evidence — the intent stays unconsumed/retired; no launch.

## R108

Agent event intake verifies envelopes, scrubs fields, applies idempotently, never launches — untrusted input is sanitized at ingest and never grants dispatch.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1; [trust boundary](execution-loop.md#six-design-aspects). Suite: LP-28.

Planned proof: inject a forged lifecycle event — rejected at ingest; a replayed event is deduplicated; neither launches a run.

## R109

Producer hooks fail open with durable queue — mechanism omitted (no hooks); retained: observation loss is bounded and recorded — a dropped read cannot fabricate an event.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1; F10. Suite: LP-18.

Planned proof: a failed read surfaces `unavailable`, is retried next pass, and never synthesizes an Observation.

## R110

Session-to-work linking only on explicit evidence — a session event cannot complete or settle a Work.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1; F-ingest dedup. Suite: LP-28.

Planned proof: a session-close event with no recorded binding is retained as evidence but changes no lifecycle state.

## R117

Event producer forwards lifecycle metadata fail-open — mechanism omitted; retained: intake is bounded, deduplicated, and fail-closed on malformed envelopes.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1; LP-28. Suite: LP-28.

Planned proof: an oversized/malformed event is refused within bounds; the loop continues.

## R119

Recovery attempts bounded, persisted before dispatch, skipped refunds — an intent consumed-but-not-dispatched refunds its charge; a crash mid-dispatch mints no second attempt.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F15. Suite: LP-25, LP-23.

Planned proof: crash between charge and launch — restart sees the charged intent; a skipped pre-dispatch outcome refunds the budget exactly once.

## R120

Owed work across ticks — pending intents survive skipped passes and are neither flushed nor erased until consumed or retired.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement). Suite: LP-25.

Planned proof: stop the loop past `due_at` — on restart the intent fires exactly once; nothing pending is dropped.

## R121

Attention withhold semantics — an item whose basis is unreadable withholds rather than reporting resolved; only genuinely-answered items acknowledge.

Clause: [Attention delivery](execution-loop.md#owner-decisions); F14. Suite: LP-22.

Planned proof: corrupt the attention basis — the item withholds/flags `unknown`, never displays as resolved.

## R123

Publish-capable dispatch verifies role identity before launch — dispatch revalidates actor/capability binding per [installation config](installation-config.md).

Clause: [phases](execution-loop.md#tick-phases-in-order) 2; [installation config](installation-config.md#roles-and-permission-matrix). Suite: LP-04.

Planned proof: remove the publication role binding — dispatch holds with `unsupported`/`denied`, never launches unbound.

## R124

Halt/attention items derive only from recorded evidence under the Store's own identity — foreign or out-of-root records cannot source an Attention item.

Clause: [phases](execution-loop.md#tick-phases-in-order) 8; [trust boundary](execution-loop.md#six-design-aspects). Suite: LP-22.

Planned proof: plant a foreign-record attention seed — the derivation ignores it; only store-owned evidence produces items.

## R125

Persistent idempotent superseding judgment-wait — an open Attention item is idempotent, superseded by newer versions, and always answerable until resolved.

Clause: [command surface](execution-loop.md#command-surface) `acknowledge`; [phases](execution-loop.md#tick-phases-in-order) 8. Suite: LP-22.

Planned proof: two derivations of the same unresolved prerequisite yield one item; a newer version supersedes; the operator can always answer the current version.

## R126

Judgment carry-over only under proven same-context — prior answers re-record only when the evidence generation provably matches; a new head/finding set requires a fresh decision.

Clause: F8; [command surface](execution-loop.md#command-surface). Suite: LP-16.

Planned proof: change the finding set — prior decisions cannot silently carry; a fresh judgment is required.

## R127

Pre-launch admission serializes capacity→claim→spawn — under the store's atomic guarantees; deferral on resource evidence holds without partial launch.

Clause: [phases](execution-loop.md#tick-phases-in-order) 2; F1. Suite: LP-10.

Planned proof: race two dispatches for one slot — exactly one wins the transaction; the loser defers or rejects, never half-launches.

## R131

Single-channel failure surfacing — mechanism omitted (no cron wrapper); retained: a failed pass reports its failure once via the loop's own output and exit status.

Clause: [loop model](execution-loop.md#loop-model). Suite: LP-04.

Planned proof: fail phase 4 — `run --once` exits non-zero with one structured report; no double reporting channel.

## R132

Mutating operator commands serialize with the loop — mechanism re-modelled: mutations transact atomically against the store; they never race a tick because ordering is transactional, not lock-based.

Clause: [loop model](execution-loop.md#loop-model) run lock note; [command surface](execution-loop.md#command-surface). Suite: LP-03, LP-10.

Planned proof: submit during a dispatch commit — the Command sees the committed state; no torn observation.

## R133

Exclusion-comment reconciliation — mechanism omitted (no exclusion comments); retained: baseline/dedup rules for external records map onto Observation dedup.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1. Suite: LP-28.

Planned proof: replay identical external events — one ingest receipt, no duplicate effect.

## R134

Delivered-vs-observed evidence separation — acknowledgment state is recorded separately from the item's existence; an audit names what was actually presented.

Clause: [Attention delivery](execution-loop.md#owner-decisions). Suite: LP-22.

Planned proof: acknowledge then re-derive — the item's record shows both observed and acked state distinctly.

## R135

Generated-path exclusion preserves files — mechanism omitted (no cache-exclusion playbook); retained: cleanup touches only recorded-managed bodies per [retention policy](retention-policy.md).

Clause: [retention policy](retention-policy.md#cleanup-eligibility-and-failure-matrix). Suite: cited RT suite.

Planned proof: an unmanaged file next to a managed body is never reclaimed.

## R136

Equivalent-task scan dispatches only on proven identity overlap — an unreadable source is a structured refusal, never an assumed overlap or non-overlap.

Clause: F1; [identity storage](identity-storage.md#concrete-identity-model). Suite: LP-10.

Planned proof: an unverifiable source during admission holds dispatch as `unknown`, not conflict and not admission.

## R137

Retry enqueue replays only provenance-marked fields — a requeue carries recorded request fields; nothing re-pins a legacy implicit mode.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement). Suite: LP-25.

Planned proof: inspect a requeued intent — every field traces to the recorded request; no ambient config is re-read into the task.

## R138

Deterministic successor re-runs full intake — a requeue revalidates the complete current evidence set and pins to the recorded binding.

Clause: [phases](execution-loop.md#tick-phases-in-order) 6. Suite: LP-25.

Planned proof: requeue after a config change — the successor revalidates against the recorded binding, not silently updated fields.

## R139

System-vs-operator park distinguished by stamped evidence — re-check on resume re-measures the configured floor; unknown fails closed.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F14. Suite: LP-25.

Planned proof: distinguish a policy-hold from an operator hold — only the former's auto-resume path exists, and only with fresh evidence.

## R140

Pre-launch reenqueue admits only proven terminal environment failure — no-execution-record retries require classified terminal evidence.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F15. Suite: LP-25.

Planned proof: a non-terminal or unclassified failure cannot requeue; only the closed terminal-env class may.

## R141

Terminal quiescence reconciles every recorded binding — settlement requires all intents, writers, and effects resolved.

Clause: [timeout recovery](execution-loop.md#timeout-recovery-sequence); F15. Suite: LP-23.

Planned proof: settle while an outbox intent is unresolved — terminal transition refuses until the intent reconciles.

## R142

Episode-keyed audit with normalized data — Attention/recovery records key to their episode and carry bounded normalized fields; a re-alert starts a fresh budget.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); [envelope](execution-loop.md#envelope) `reason`. Suite: LP-22, LP-28.

Planned proof: a second failure episode mints a new attempt budget keyed to the new episode; prior episode records remain history.

## R143

Deferred dispatches and never-lost escalation — pass overflow defers whole items to the next pass; crashed phases surface as needs-human; an escalation record is never lost.

Clause: [loop model](execution-loop.md#loop-model) tick deadline; [phases](execution-loop.md#tick-phases-in-order) 8. Suite: LP-04, LP-22.

Planned proof: overrun the tick deadline — remaining items defer intact; a crashed phase reports rather than silently skipping its escalation.

## R144

Fail-closed config — covered by the [installation configuration](installation-config.md#configuration-schema): parse errors collect, unknown keys record, absent `enabled` means disabled.

Clause: [installation config](installation-config.md#configuration-schema). Suite: cited CF suite.

Planned proof: malformed TOML fails load without mutation; a guessed default never results.

## R145

Persisted selection artifact then bounded spawn — the durable request is recorded before any launch; spawn failures classify, never raise.

Clause: F2; [phases](execution-loop.md#tick-phases-in-order) 2. Suite: LP-10, LP-11.

Planned proof: fail the spawn — the persisted request reconciles; the failure classifies into the typed vocabulary.

## R147

Deadline-bounded single-flight pass with durable outcome — the run lock, tick deadline, and per-pass report satisfy this directly.

Clause: [loop model](execution-loop.md#loop-model). Suite: LP-03, LP-04.

Planned proof: every pass leaves a durable tick report; concurrent loops refuse; deadline overrun defers rather than overruns.

## R148

Overlapping pass resolves to a skip via non-blocking lock — the run lock refuses a second loop; non-contention lock failures are fatal, never queued.

Clause: [loop model](execution-loop.md#loop-model) run lock. Suite: LP-03.

Planned proof: hold the lock — a second `run` fails closed; a lock-substrate error aborts, it does not queue a phantom loop.

## R149

Park clearing only on proven terminal evidence — a held state clears only on a proven terminal read, recorded as an in-place evidence mark, never a delete.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F14. Suite: LP-25.

Planned proof: a partial/ambiguous terminal read cannot clear the hold; only a proven terminal record marks it.

## R150

Persisted diagnostic references self-certifying — Attention/evidence items carry bounded self-describing references rebuilt from recorded fields.

Clause: [envelope](execution-loop.md#envelope) `evidence_refs`; [phases](execution-loop.md#tick-phases-in-order) 8. Suite: LP-22.

Planned proof: a referenced evidence item resolves by recorded identity only; a foreign path cannot stand in.

## R151

Held state survives key-scheme upgrade — re-modelled: holds key by versioned identity/generation, so a schema version bump cannot silently unhold.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement) generation field. Suite: LP-25.

Planned proof: bump the intent schema version — pending holds rebind by versioned key and still fire/retire correctly.

## R152

Held items never reorder the queue — holds are per-Work state; enqueue order is committed order and never reordered by holds.

Clause: [phases](execution-loop.md#tick-phases-in-order) 2 (enqueue order). Suite: LP-04.

Planned proof: hold W1 — W2's queue position is unchanged; dispatch order remains commit order.

## R153

Capability-gated selection — mechanism omitted (no reselect); retained: admission/dispatch is capability-gated per [installation config](installation-config.md).

Clause: [phases](execution-loop.md#tick-phases-in-order) 2; [installation config](installation-config.md#roles-and-permission-matrix). Suite: LP-04.

Planned proof: an undeclared capability at dispatch — `unsupported`, no launch.

## R154

Read-only bounded per-item reconciliation — probe results classify evidence and attach bounded diagnostics; no mutation during reconcile.

Clause: F6; [phases](execution-loop.md#tick-phases-in-order) 4. Suite: LP-15.

Planned proof: reconcile a held item — the pass mutates nothing; classifications are recorded.

## R155

Per-candidate conflict probing — proven live work excludes; an incomplete scan holds as a distinct outcome, never assumed clear.

Clause: F1; [phases](execution-loop.md#tick-phases-in-order) 2. Suite: LP-10.

Planned proof: an unscannable resource during admission — dispatch holds `unknown`, not admitted and not falsely conflicting.

## R157

Serialized reconcile pass settling under owning locks — the loop is single-flight; each Work settles under its own transaction; dependent sweeps run after settlements.

Clause: [loop model](execution-loop.md#loop-model); [phases](execution-loop.md#tick-phases-in-order) order. Suite: LP-04.

Planned proof: a dependent sweep never runs ahead of its settlement — phase order guarantees settle-then-sweep.

## R159

Re-home only on attributable evidence — re-mapping work to a lane/owner requires recorded attributable evidence; each confirmed mutation precedes the next.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement). Suite: LP-25.

Planned proof: an unattributable demotion cannot re-home the Work; each recorded step is durable before the next.

## R160

Bounded replan mints at most one deterministic successor — planner machinery is deferred; retained: any successor Work is bounded, deterministic, and carries only still-valid scope.

Clause: [post-v0.1 milestones](post-v0.1-milestones.md#planning-and-bounded-replanning); [timeout recovery](execution-loop.md#timeout-recovery-sequence). Suite: LP-25.

Planned proof: v0.1 has no replan path; a requeue is the only automatic successor and is bounded to one per intent.

## R161

Deferred launch parks a minted identity through a bounded pending budget — a queued-but-unadmitted Execution persists with typed hold reasons under bounded time.

Clause: [phases](execution-loop.md#tick-phases-in-order) 2; F14. Suite: LP-10, LP-22.

Planned proof: defer dispatch on capacity — the queued Execution persists with its reason; prolonged deferral escalates rather than vanishing.

## R163

Reused idempotency key on different work fails loudly — `idempotency_conflict`; terminal dedup collisions re-key bounded times.

Clause: [envelope](execution-loop.md#envelope); F1. Suite: LP-10, LP-06.

Planned proof: reuse a key with a changed payload — `idempotency_conflict`, original retained, exit 1.

## R164

Deadline continuation — a worker-reported deadline gets at most one lineage-bounded continuation before generic handling strands it.

Clause: [timeout recovery](execution-loop.md#timeout-recovery-sequence); F15. Suite: LP-23.

Planned proof: deadline with saved-point evidence — one bounded requeue; a second deadline follows the generic exhaustion path.

## R165

Stale ready items superseded by newer covering work — a stale queued/runnable item yields to the newest recorded specification covering the same identity.

Clause: F1; [phases](execution-loop.md#tick-phases-in-order) 2. Suite: LP-10.

Planned proof: supersede a ready Execution with a newer revision — the older is marked superseded and never launches.

## R166

Ordered deadline-bounded pass with per-item lock — the tick is one ordered pass; per-Work processing is isolated under its own transaction.

Clause: [loop model](execution-loop.md#loop-model); [phases](execution-loop.md#tick-phases-in-order). Suite: LP-04.

Planned proof: one Work's processing error cannot hold others; the pass respects its deadline and ordering.

## R167

Idempotent enqueue lifecycle — same-work key proof, bounded re-key, crash recovery, verified binding.

Clause: F1; [envelope](execution-loop.md#envelope). Suite: LP-10, LP-06.

Planned proof: crash between enqueue decision and commit — the persisted key resumes; a duplicate submit returns the original decision.

## R168

Claimant-only liveness across blocking stages — process liveness is judged only by recorded claimant identity; worker-protocol supervision owns the mechanics.

Clause: F3/F4; [worker protocol](worker-protocol.md#process-descendants-and-stop-proof). Suite: LP-11, LP-12.

Planned proof: a foreign process with a recycled PID cannot prove liveness; heartbeat loss alone never kills.

## R170

Serialized launch/reattach per work item — live-PID reattach, dead-record advance, never-started fresh dispatch, revalidate-then-launch.

Clause: F2; [phases](execution-loop.md#tick-phases-in-order) 3. Suite: LP-11.

Planned proof: restart with a live recorded process — reattach; a dead record advances to reconcile; an unlaunched request dispatches fresh after revalidation.

## R171

Non-blocking watch under claim pulse — supervision polls without blocking the pass; terminal exits and timeouts classify fail-closed.

Clause: [phases](execution-loop.md#tick-phases-in-order) 3. Suite: LP-04, LP-12.

Planned proof: a long-running Invocation doesn't stall the pass — its observation defers to next tick within bounds.

## R172

Fail-closed validation before any effect — outbox items revalidate artifact/intent before any write; one guarded effect per actionable item.

Clause: [phases](execution-loop.md#tick-phases-in-order) 4; F6. Suite: LP-15.

Planned proof: an invalid pending intent — refused before any remote write; at most one guarded effect per item per pass.

## R173

Honest resume promises — resume claims continuation only where durable evidence supports it; otherwise it reports the gap.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); F13. Suite: LP-25.

Planned proof: resume with a missing continuation record — rejection names the absent evidence, never a fabricated "resumable".

## R178

Terminal remote cleanup fenced retries — mechanism omitted (no remote destroy); retained: cleanup retries are bounded and fenced by recorded state (retention's two-phase reclaim).

Clause: [retention policy](retention-policy.md#scheduling-and-ownership). Suite: cited RT suite.

Planned proof: a failed delete retries bounded times under the `reclaiming` fence; three consecutive failures escalate.

## R179

Orphan open record settles from recorded evidence — a leftover record resolves by its own recorded evidence; unknown stays held.

Clause: F4; [phases](execution-loop.md#tick-phases-in-order) 3. Suite: LP-13.

Planned proof: an orphaned run record with terminal evidence settles; without it, the record holds unresolved — never force-settled.

## R180

Bounded read adapter fails closed — a timeout/nonzero/malformed read is `unknown`; negative answers never become positive.

Clause: F10; [phases](execution-loop.md#tick-phases-in-order) 1/4. Suite: LP-15, LP-18.

Planned proof: malformed adapter output — `unknown`, never parsed into a success; a missing answer cannot fabricate one.

## R185

Post-completion auto-continue — mechanism omitted (no auto-continue); retained: progression after completion happens only through queue admission of already-accepted Work.

Clause: [phases](execution-loop.md#tick-phases-in-order) 2. Suite: LP-10.

Planned proof: completing W1 launches nothing implicitly; W2 proceeds only through normal admission.

## R188

Fixed-vocabulary denied-operation diagnosis — bounded grammar, correlated evidence, categorized write targets.

Clause: [envelope](execution-loop.md#envelope) `reason.code` closed set; [sanitization](execution-loop.md#planned-verification-suite). Suite: LP-28, LP-02.

Planned proof: a denied effect reports a closed reason code with bounded diagnostics and retained evidence refs.

## R189

Owner-only bounded self-certifying failure artifacts — retained via bounded retained evidence; the self-certifying mint mechanism is omitted.

Clause: [evidence classes](execution-loop.md#evidence-classes); LP-28. Suite: LP-28.

Planned proof: failure evidence is size-bounded, owner-visible, and self-describing by recorded identity — no freeform payload.

## R191

Launch fixes the machine contract — the Invocation's immutable input is the contract; worker-protocol owns the prompt/task mechanics.

Clause: [worker protocol](worker-protocol.md#schemas); [phases](execution-loop.md#tick-phases-in-order) 2. Suite: LP-10.

Planned proof: the launched task carries its fixed contract; ambient environment cannot rewrite it.

## R192

Durable honest ledger — writes are atomic; reads tolerate but never silently accept malformed records.

Clause: [loop model](execution-loop.md#loop-model) crash safety; [evidence classes](execution-loop.md#evidence-classes). Suite: LP-05, LP-28.

Planned proof: corrupt a record — read classifies it `invalid` and retains it; writes are atomic commits.

## R193

Post-PR review convergence on actionable-thread evidence — owned by [evidence gates](evidence-gates.md); the loop supplies bounded fix/re-review scheduling.

Clause: [evidence gates](evidence-gates.md); [phases](execution-loop.md#tick-phases-in-order) 5–6. Suite: LP-26.

Planned proof: findings bound to launched thread IDs converge within bounds; unresolved findings escalate.

## R206

Marker-delimited structured judge output — owned by [worker protocol](worker-protocol.md); failures retain bounded raw output and name it.

Clause: [worker protocol](worker-protocol.md#schemas); [envelope](execution-loop.md#envelope). Suite: LP-28.

Planned proof: unparseable worker output — refused at ingest; the bounded raw dump is retained and referenced.

## R208

Validate-before-persist for any observation record — mechanism (snapshot cache) omitted; retained: an Observation validates its versioned contract before any persisted byte.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1. Suite: LP-28.

Planned proof: a malformed Observation commits nothing; the failure records and the state is untouched.

## R209

Unsubmitted-action quit guard — mechanism omitted (console); retained: CLI mutations are synchronous — no unsubmitted-queue state can exist.

Clause: [command surface](execution-loop.md#command-surface). Suite: LP-01.

Planned proof: there is no buffered-mutation path; a Command either committed its decision or never ran.

## R210

Found/Absent/Unusable observation classification — Query results are three-valued: a known absence names the identity; foreign/malformed is unusable, not absent.

Clause: [envelope](execution-loop.md#envelope) queries. Suite: LP-07.

Planned proof: query a missing Work (`accepted`+null), a foreign ID (rejected), and an unreadable store (exit 4) — three distinct outcomes.

## R211

Published contract constants — `schema_version` and the closed reason vocabulary are the compiled contract; consumers verify parity against the documented set.

Clause: [envelope](execution-loop.md#envelope); [exit codes](execution-loop.md#exit-codes). Suite: LP-01, LP-02.

Planned proof: the documented vocabulary and emitted codes stay in lockstep; an undocumented code fails contract tests.

## R212

Non-blocking reads — mechanism (TUI) omitted; retained: Queries never block on the loop and read independent of in-flight processing.

Clause: [loop model](execution-loop.md#loop-model); [command surface](execution-loop.md#command-surface). Suite: LP-03.

Planned proof: query while a pass is mid-phase — the Query returns committed state; it neither waits nor observes torn data.

## R213

Terminal-bound sanitization — untrusted strings are control-stripped and bounded before display.

Clause: [envelope](execution-loop.md#envelope); LP-28. Suite: LP-28.

Planned proof: inject ANSI/bidi/zero-width sequences into evidence text — output strips/escapes them; raw bytes never reach the terminal.

## R215

Atomic per-record write-back, marker advances after the batch — Observation ingest is atomic per event; the refresh checkpoint advances only after the batch commits.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1. Suite: LP-04.

Planned proof: crash mid-batch — committed events are deduplicated on re-read; the checkpoint never claims uncommitted progress.

## R216

Proven-fresh freshness band — a source read counts fresh only within its timestamp band; stale or ambiguous is not fresh.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1; F10. Suite: LP-18.

Planned proof: serve an out-of-band cached read — it cannot satisfy a fresh-evidence precondition.

## R217

Single serialized read backend — mechanism (console) omitted; retained: the loop is the only driver; Queries read committed state directly.

Clause: [loop model](execution-loop.md#loop-model). Suite: LP-03.

Planned proof: concurrent Queries and a running pass cannot interleave into a torn view — reads see committed snapshots.

## R218

Frozen exit-code contract — the loop-side obligation is the documented 0–6 vocabulary; adapter/class failures map onto it without inventing codes.

Clause: [exit codes](execution-loop.md#exit-codes). Suite: LP-02.

Planned proof: every staged fault class lands on its documented code; no code outside 0–6 is emitted.

## R219

Closed error vocabulary with bounded flattened diagnostics — untrusted stderr/protocol text flattens to one bounded sanitized line.

Clause: [envelope](execution-loop.md#envelope) `reason`; LP-28. Suite: LP-28, LP-02.

Planned proof: a hostile multi-KiB stderr — one bounded line in `reason`, full text only via retained evidence ref.

## R220

MCP read-only catalog — mechanism deferred (MCP optional); retained: the Query surface is a closed read-only set with strict input validation.

Clause: [post-v0.1 milestones](post-v0.1-milestones.md); [command surface](execution-loop.md#command-surface). Suite: LP-07.

Planned proof: v0.1 ships no MCP surface; Queries reject unknown fields/verbs fail-closed.

## R221

Read-only projection CLI contract — Queries form a fixed subcommand allowlist with fail-closed usage errors and bounded external reads.

Clause: [command surface](execution-loop.md#command-surface); [exit codes](execution-loop.md#exit-codes). Suite: LP-02, LP-07.

Planned proof: an unknown query verb exits 2; a query performs no mutation and bounded reads only.

## R223

Diagnose→refresh→promote launch sequence — mechanism omitted (console); retained: `check` preflights before `run` can depend on the installation; a failed check aborts, it does not degrade.

Clause: [installation config](installation-config.md#setup-contract); [loop model](execution-loop.md#loop-model). Suite: LP-04, cited CF suite.

Planned proof: run with a failed prerequisite — the loop refuses at startup, not mid-pass.

## R224

Deterministic read-only current-state projection — Queries are deterministic projections of recorded state; each source fails independently into a labelled per-source diagnosis.

Clause: [envelope](execution-loop.md#envelope) queries; F10. Suite: LP-07.

Planned proof: two sources, one failing — the Query labels the failed source distinctly and returns the rest.

## R225

Closed-vocabulary evidence-only diagnostics — every not-fully-known unit carries a bounded reason from the closed set; no prose invention.

Clause: [envelope](execution-loop.md#envelope); [exit codes](execution-loop.md#exit-codes). Suite: LP-07, LP-28.

Planned proof: an indeterminate unit renders `unknown` + bounded reason — never an inferred explanation.

## R226

Escalation-only output whitelist — the loop prints nothing on a clean pass; only actionable events surface.

Clause: [loop model](execution-loop.md#loop-model). Suite: LP-04.

Planned proof: a quiet pass emits no spurious records; an actionable transition emits exactly one structured event.

## R227

Untrusted identifiers control-stripped and bounded — session/run/source identifiers are sanitized before reaching any agent-readable surface.

Clause: [envelope](execution-loop.md#envelope); LP-28. Suite: LP-28.

Planned proof: a control-laden external identifier — stripped/bounded in output, never passed through.

## R228

Lock-busy drops journaled and drained — a skipped pass's owed work persists and the next pass drains it; a missed signal never strands a pending intent.

Clause: [persisted scheduling](execution-loop.md#persisted-scheduling-and-retirement); [loop model](execution-loop.md#loop-model). Suite: LP-04, LP-25.

Planned proof: lose a whole tick — pending intents remain owed and drain on the next pass, exactly once each.

## R231

One-shot deadline-bounded read-only probe — `check`/diagnose-style commands are read-only and their verdicts never feed dispatch.

Clause: [command surface](execution-loop.md#command-surface); [installation config](installation-config.md#setup-contract). Suite: LP-07, cited CF suite.

Planned proof: a probe result cannot authorize dispatch; only the admission path can.

## R234

Source queries scoped exactly to the registered set — refresh reads only declared sources; broader activity never substitutes; watched items are never mutated by reads.

Clause: [phases](execution-loop.md#tick-phases-in-order) 1. Suite: LP-04.

Planned proof: an undeclared repository's activity — never ingested; reads mutate no watched state.

## R235

Operator action is thin orchestration — mandatory fresh re-fetch and read-only re-verification gate every intervention.

Clause: [preconditions](execution-loop.md#identities-and-preconditions-for-mutation). Suite: LP-06.

Planned proof: an intervention on stale displayed state — the acceptance recheck rejects with the current values.

## R236

Status answers derive solely from the deterministic Query — sessions/process lists/logs are never progress evidence; honest `unknown` is first-class.

Clause: [envelope](execution-loop.md#envelope); F11. Suite: LP-07, LP-19.

Planned proof: a Query result is the sole status source — a live-looking process cannot substitute for recorded progress.

## R237

Closed multi-registry config schema — covered by the [installation configuration](installation-config.md#configuration-schema) allowlist/validation rules.

Clause: [installation config](installation-config.md#configuration-schema). Suite: cited CF suite.

Planned proof: unknown keys fail load; lane/policy defaults are the closed documented set.

## R238

Sweep-detected deadline — identity-checked stop, persisted outcome, transient block, idempotent terminal sync, bounded handoff.

Clause: [timeout recovery](execution-loop.md#timeout-recovery-sequence); F15. Suite: LP-23.

Planned proof: deadline detected by the sweep — the ordered sequence runs with each step durable; handoff bounds respected.

## R242

Staleness as derived non-destructive overlay — aged non-terminal records render stale, never completed; the stored record is never mutated by display.

Clause: [envelope](execution-loop.md#envelope) queries. Suite: LP-07.

Planned proof: an aged in-flight Execution displays `stale`, not `completed`; underlying records are untouched.

## R255

Prepare binds live snapshot under fail-closed refusal — submission binds the live source snapshot/HEAD to the repository profile; unverifiable binding refuses.

Clause: [identity storage](identity-storage.md#concrete-identity-model); [phases](execution-loop.md#tick-phases-in-order) 2. Suite: LP-10.

Planned proof: an unverifiable source binding at submit/dispatch — refuse fail-closed; bound snapshot travels with the Work.

## R275

Bounded single-shot review chain with typed terminal outcome — review Invocations are bounded and produce typed terminal results; evidence-gates/worker-protocol own the semantics, the loop schedules them.

Clause: [phases](execution-loop.md#tick-phases-in-order) 3/5; [evidence gates](evidence-gates.md). Suite: LP-23.

Planned proof: a review Invocation exceeding its bound stops via the ordered sequence with typed terminal outcome.

## R316

Three-valued remote terminal probe — only positive merged evidence is `merged`; `unknown` is never coerced into a safe-to-act answer.

Clause: F6/F7; [phases](execution-loop.md#tick-phases-in-order) 4. Suite: LP-15.

Planned proof: an ambiguous remote state — reconciles as `unknown`, never as merged/unmerged; no dependent write proceeds.
