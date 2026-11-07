# Worker contract requirement mapping

Status: normative target obligations for Issue #52, derived from the complete pinned inventory in [extraction coverage](extraction-coverage.md). The protocol, handoff, isolation, and suite clauses are specified in [worker protocol](worker-protocol.md). This is documentation only. No requirement below claims runtime implementation, a fixed defect, live host behavior or executed acceptance.

## Reading and mapping rules

Each R heading is a precise target for the #52-owned inventory link. The scope is that requirement's Worker-owned obligation — the part the Worker protocol, artifact handoff, or isolation contract must satisfy — not its entire source mechanism. Obligations owned by evidence gates (#51), storage and retention (#54), or the loop (#55) are cited, not duplicated.

Every mapping names the governing clause and the shared Worker contract suite cases that prove it. Where the retained safety obligation is met by a different mechanism than the source's — sandbox enforcement in place of a server-side audit, supervisor-run verification in place of worker self-report, namespace teardown in place of session release — the substitution is recorded explicitly. A deferral cites the owner-approved milestone scope; a deferred requirement remains a committed obligation, not a dropped one.

The proof paragraphs are planned contract scenarios. Run them through the future public Command, Query and adapter paths with literal expected decisions, retained evidence and effects. Structural schema checks supplement those scenarios; they cannot establish live behavior. This document's 33 requirement headings cover all 33 #52 inventory targets at this revision. Counts can be regenerated with the extraction checker and by counting targets whose `owner_issue` is 52 in `docs/extraction-coverage.json`.

## R11

Lost start acknowledgement is uncertainty, not failure. A replayed request with an existing launch intent never spawns again; a lost start response reconciles the recorded process identity or leaves the Invocation `uncertain`. The Worker-owned slice covers start idempotency and launch facts; Work-wide successor and budget limits follow #51.

Clause: [start, observe, stop](worker-protocol.md#start-observe-stop); [failure mapping](worker-protocol.md#failure-mapping). Suite: WS-02, WS-03, WS-05.

Planned proof for AC2: Deliver the same request twice and lose the start response once; exactly one Invocation persists across a supervisor restart with no second spawn.

## R13

Immutable bounded Invocation. The request fixes task, base, budget, verification argv, and capability requirements before dispatch. Adapter-local retries are declared in `WorkerCapabilities.local_retries`, bounded by the deadline and `max_turns`, and never create new Invocations, commits outside the workspace, or remote effects.

Clause: [schemas](worker-protocol.md#schemas); [adapter-local retries](worker-protocol.md#adapter-local-retries). Suite: WS-17, WS-22.

Planned proof for AC5: Restart between charging and launching a request — the saved charge is counted once; an adapter retry beyond its declared bound has no path and is recorded.

## R14

Worker claims are evidence, never outcome. A completed report still requires durable handoff, supervisor verification, publication, and writer-safety proof; a malformed or absent report cannot become an empty success.

Clause: [roles and authority](worker-protocol.md#roles-and-authority); [evidence validity](worker-protocol.md#evidence-validity). Suite: WS-08, WS-09, WS-10.

Planned proof for AC2: A completed report with missing handoff and stop evidence cannot close the Work; the report is retained with its missing-evidence reason.

## R15

Stop proof and uncertainty. Stop intent persists before signaling; every recorded descendant carries reuse-resistant identity; missing liveness, a dead leader, or a worker summary cannot prove the tree stopped. The PID-namespace init adds the stronger proof: init exit kills every namespace descendant.

Clause: [process descendants and stop proof](worker-protocol.md#process-descendants-and-stop-proof). Suite: WS-06, WS-07.

Planned proof for AC5: Fail intent persistence and assert no signal; leave a live `setsid` child after leader exit and retain the slot; inject a probe outage and retain `uncertain`.

## R16

Optional planning evidence — deferred. The owner-approved planner-machinery deferral ([post-v0.1 milestones](post-v0.1-milestones.md#planning-and-bounded-replanning)) covers this requirement: no planner runs in v0.1. The retained obligation: if planning evidence is ever supplied, it binds the immutable request, a claimed read-only phase is distinguished from enforced permissions, and a plan can never satisfy verification or review.

Clause: the deferral above; the read-only binds in [role translation](worker-protocol.md#role-translation-for-the-reference-adapter) already enforce what existing roles may write. Suite: none in v0.1.

## R17

Exact retained change and verification. Verification binds retained git objects, the produced commit, the manifest digest, argv, and environment identity; an ephemeral path, a hash without bytes, or a result from another artifact cannot satisfy the requirement.

Clause: [artifact handoff](worker-protocol.md#artifact-handoff); [verification runner](worker-protocol.md#verification-runner). Suite: WS-01, WS-13, WS-22.

Planned proof for AC2: Remove required object bytes and deny completion while preserving recovery references; alter the manifest digest or argv and reject the old result.

## R18

Typed failure classification. Structured request, evidence, and stop facts classify independently of process exit status: a known refusal, incomplete collection, a valid negative check, and an unknown write need distinct persisted reasons and different allowed recovery actions.

Clause: [schemas](worker-protocol.md#schemas) (`BoundedReasonCode`); [failure mapping](worker-protocol.md#failure-mapping). Suite: WS-17.

Planned proof for AC5: Exit zero carrying a human-decision report and a nonzero transport error after an ambiguous write produce distinct classes — neither success nor a blind replacement.

## R21

Publication actor binding is not Worker-owned. The Worker holds no repository-host credentials and performs no remote effects, so no Worker-side actor binding exists that renewal could silently change. The retained obligation — an outstanding action's actor survives credential renewal — lives in #51's stage-effect intents and #53's credential scope. This mapping records that the Worker contract contributes the structural guarantee: worker-side publication is impossible.

Clause: [roles and authority](worker-protocol.md#roles-and-authority); [selected profile](worker-protocol.md#selected-profile) (credentials). Suite: WS-15, WS-19.

Planned proof for AC5: A host write attempted from inside the sandbox fails for lack of credentials; a credential rotation between dispatches cannot alter a running Invocation's environment.

## R24

Bounded reasons and retained diagnostics. Terminal classifications use the closed `BoundedReasonCode` set; provider text lives only in the bounded `runtime.log`, never in reason fields or Attention text; invalid evidence retains its exact reason and identity.

Clause: [schemas](worker-protocol.md#schemas); [evidence validity](worker-protocol.md#evidence-validity). Suite: WS-17.

Planned proof for AC5: An oversized credential-bearing provider error is redacted to a bounded reason; the full text reaches only the bounded retained log, and a superseded refusal cannot override a newer authoritative result.

## R25

Exact review target. CodeReview and InvariantCheck run on a read-only checkout of the exact `DiffTarget` or artifact commit bound at dispatch; the Worker-owned slice guarantees the runtime can see and assess only that bound target. Judgment and acceptance binding remain #51's.

Clause: [role translation for the reference adapter](worker-protocol.md#role-translation-for-the-reference-adapter). Suite: WS-18.

Planned proof for AC5: A review report against a stale head cannot settle a newer head; findings bind the sealed snapshot and nothing else.

## R30

Read-only reconciliation. Re-observation uses the recorded process identity and the out directory; an unreadable record or ambiguous match stays unknown and authorizes no write and no replacement.

Clause: [start, observe, stop](worker-protocol.md#start-observe-stop); [crash recovery](worker-protocol.md#crash-recovery). Suite: WS-03, WS-11.

Planned proof for AC5: Crash after a confirmed exit before the result is recorded; on restart, re-observation from the out directory is idempotent by Invocation ID.

## R32

Recorded bounded recovery. Each export/import recovery attempt records its charge against the artifact-recovery bound — separate from `max_invocations`; missing, corrupt, and wrong-subject evidence are distinct reasons; exhaustion preserves retained evidence and never erases an unknown writer.

Clause: [crash recovery](worker-protocol.md#crash-recovery); [failure mapping](worker-protocol.md#failure-mapping). Suite: WS-08, WS-11.

Planned proof for AC5: Inject missing then corrupt bundle evidence and a crash after charging; assert distinct reasons, bounded recollection, and no budget reset.

## R36

Visible unknown sources. `Unknown` start, absent report, truncated log, and invalid report are distinct retained states; none may be presented as a completed result or an empty complete set.

Clause: [evidence validity](worker-protocol.md#evidence-validity). Suite: WS-05, WS-08, WS-09.

Planned proof for AC5: A failed report read plus a missing log surfaces partial/unknown — never zero findings or success.

## R37

Effective capability evidence. `describe` binds the exact adapter ID, version, protocol versions, roles, and capability digest into the request; dispatch additionally requires the recorded preflight probes to pass. A runtime version differing from the declaration is a configuration repair, not a silent upgrade; an installation message alone proves no capability.

Clause: [entry point and files](worker-protocol.md#entry-point-and-files); [preflight](worker-protocol.md#preflight). Suite: WS-21.

Planned proof for AC3: A runtime version differing from the stored capability digest refuses start; a failed or skipped probe refuses dispatch.

## R38

Secret-safe evidence. Runtime credentials resolve through a reference into an environment variable or a read-only file; values never enter argv, request files, logs, or evidence. No repository-host credential, credential helper, SSH agent socket, or keyring enters the sandbox.

Clause: [selected profile](worker-protocol.md#selected-profile) (credentials); [schemas](worker-protocol.md#schemas). Suite: WS-15.

Planned proof for AC5: Seed credentials in environment and error text; argv and all exposed evidence contain none; failed resolution requires repair with no anonymous fallback.

## R44

Runtime credential binding per request. The supervisor resolves a credential reference at dispatch and injects only that credential; a running Invocation's environment is then fixed, and renewal between dispatches affects only future requests. No actor-carrying credential can rotate beneath an in-flight effect because Workers perform no remote effects.

Clause: [selected profile](worker-protocol.md#selected-profile) (credentials); [start, observe, stop](worker-protocol.md#start-observe-stop) (config-only swap). Suite: WS-20.

Planned proof for AC5: Rotate the runtime credential after dispatch — the running Invocation keeps its bound environment, and the next request resolves the renewed reference.

## R65

Bounded evidence collection with a permanent-refusal fast path. Report and artifact recovery proceed within their declared bounds; a `provider_refused` report is permanent for the task, consumes no recovery attempt, and retains its record; exhaustion preserves the recorded state, retained evidence, and honest flags.

Clause: [failure mapping](worker-protocol.md#failure-mapping); [crash recovery](worker-protocol.md#crash-recovery). Suite: WS-08, WS-17.

Planned proof for AC5: A permanent refusal blocks immediately without consuming the artifact-recovery bound; on recovery-bound exhaustion the record keeps the observed state, and a later attempt resumes the same Invocation identity.

## R78

Worker input is supervisor-materialized with an explicit delivery verdict. `review_source`, `invariant_set`, and goal snapshots are retained material the supervisor binds into the request; the Worker never fetches remote input itself. An absent, stale, or undeliverable snapshot makes the request invalid and stops dispatch — delivery is a checked fact, never assumed.

Clause: [role translation for the reference adapter](worker-protocol.md#role-translation-for-the-reference-adapter); [evidence validity](worker-protocol.md#evidence-validity). Suite: WS-24.

Planned proof for AC5: Dispatch a CodeReview request with a missing source snapshot — refused before spawn; a stale snapshot is invalid, not silently refetched.

## R189

Bounded owner-visible failure artifacts — re-modelled. Diagnostic bodies live in the bounded `runtime.log` and the out directory under the installation state root, with truncation recorded; the source's minted self-certifying payload mechanism is omitted. The retained obligation — failure artifacts stay bounded, owner-scoped, and honest about their own limits — is met by the evidence-validity rules and out-directory ownership.

Clause: [evidence validity](worker-protocol.md#evidence-validity) (`runtime.log` row); [entry point and files](worker-protocol.md#entry-point-and-files). Suite: WS-08.

Planned proof for AC5: An oversized log is retained marked truncated; a missing report is `unknown report`, never a sanitized success.

## R202

Atomic artifact promotion. The manifest digest, store row, and ref update reach `acknowledged` only as a whole, under the expected-absent ref guard; any crash leaves either no ref or a recoverable `imported` row — a partial promotion is never a visible artifact, and the prior handoff state is intact after any abort.

Clause: [steps and acknowledgement](worker-protocol.md#steps-and-acknowledgement); [crash recovery](worker-protocol.md#crash-recovery). Suite: WS-11.

Planned proof for AC5: Crash between the `imported` row and the ref update — recovery recreates the ref only after re-verifying the digest; abort before promotion leaves no ref.

## R204

No silent user-state mutation at setup — re-modelled. The target has no converging installer, cron, or subscription machinery: `describe` and the preflight probes are read-only, and capability detection never writes user-owned state. The retained opt-in obligation holds: only an explicit operator action changes installation state.

Clause: [entry point and files](worker-protocol.md#entry-point-and-files); [preflight](worker-protocol.md#preflight). Suite: none — a configuration-time property. Approved re-model in the 2026-11-08 #52 session.

## R205

Capability-gated dispatch — re-modelled. An absent or non-allowlisted binary refuses before spawn (`NotStarted`); a runtime version differing from the declaration is a configuration repair. The retained obligation — dependent tooling is gated on a proven capability surface — is met by `describe`, version pinning, and preflight probes rather than a provider API-surface probe.

Clause: [entry point and files](worker-protocol.md#entry-point-and-files); [preflight](worker-protocol.md#preflight). Suite: WS-04, WS-21.

Planned proof for AC5: A missing shim binary yields `NotStarted` naming the failure; a version mismatch refuses start as configuration repair.

## R253

Independent pre-publication review under enforced read-only — re-modelled. CodeReview and InvariantCheck Invocations run against a read-only workspace bind: enforcement is the mount, not a server permission flag, so a write attempt fails at the filesystem. Deadline and outage stay distinct (`deadline_reached` vs `Unknown`); self-review is structurally absent because each Invocation is a fresh runtime process with no session memory; fail-closed findings follow #51. The source's probe/fallback machinery is replaced by sandbox enforcement plus the `describe` capability declaration.

Clause: [role translation for the reference adapter](worker-protocol.md#role-translation-for-the-reference-adapter); [failure mapping](worker-protocol.md#failure-mapping). Suite: WS-16, WS-18.

Planned proof for AC5: A write attempt inside a CodeReview Invocation is denied by the read-only bind; deadline expiry classifies as `deadline_reached` while a probe outage stays `uncertain` — never conflated.

## R262

Runtime identity and teardown hygiene — re-modelled. The source's managed-server lifecycle (audited version gate, generated passwords, private config home, credential-scrubbed teardown) is omitted. Retained obligations map to: runtime version pinning via `describe`, a fresh tmpfs `HOME` per Invocation, environment allowlisting, and PID-namespace teardown that leaves no scrubbable residue.

Clause: [entry point and files](worker-protocol.md#entry-point-and-files); [selected profile](worker-protocol.md#selected-profile); [process descendants and stop proof](worker-protocol.md#process-descendants-and-stop-proof). Suite: WS-07, WS-21.

Planned proof for AC5: A spawned environment lacks host credential variables; after teardown, no runtime config home or leaked secret persists.

## R269

Enforced read-only assessment with honest verdicts. The sandbox read-only bind enforces scope for InvariantCheck and CodeReview; a killed or timed-out runner is `Incomplete`, never `Failed` or `Passed`; findings and invariant items are claims classified by #51, not verdicts the Worker can settle.

Clause: [role translation for the reference adapter](worker-protocol.md#role-translation-for-the-reference-adapter); [verification runner](worker-protocol.md#verification-runner). Suite: WS-16, WS-22.

Planned proof for AC5: A killed check run records `Incomplete`; a write attempt under the read-only bind is denied and recorded.

## R286

Plan-phase read-only audit — deferred. No plan phase exists in v0.1; the machinery that folds tool events into allowed/miscall/violation belongs to the approved planner milestone scope ([post-v0.1 milestones](post-v0.1-milestones.md#planning-and-bounded-replanning)). The retained obligation is met structurally for the roles that do exist: the sandbox bind — not an audit — makes out-of-scope writes impossible.

Clause: the deferral above; [selected profile](worker-protocol.md#selected-profile). Suite: WS-14 covers write denial; no planner-specific case in v0.1.

## R287

No leaked execution residue — re-modelled. The source's released-run sessions have no analog: one Invocation is one process tree; every terminal path ends in namespace teardown, and the recorded descendant snapshot is the leak ledger — a surviving identity keeps the Execution `uncertain` instead of being silently dropped.

Clause: [process descendants and stop proof](worker-protocol.md#process-descendants-and-stop-proof). Suite: WS-06, WS-07.

Planned proof for AC5: Success, failure, cancellation, and deadline paths each end with every recorded identity stopped or `uncertain`; a `setsid` escape is killed with the namespace init.

## R288

Plan-phase output privacy — deferred. The plan-phase machinery is in the same approved milestone scope. The retained obligation — bounded sanitized evidence — is already enforced: provider prose reaches only the bounded `runtime.log`, reason fields carry closed codes, and evidence fields are schema-bound.

Clause: [post-v0.1 milestones](post-v0.1-milestones.md#planning-and-bounded-replanning); [schemas](worker-protocol.md#schemas). Suite: none planner-specific in v0.1.

## R307

Two-phase plan-execute dispatch — deferred under the approved planner-machinery scope. v0.1 dispatches a single-phase task. When the milestone lands, a plan must bind the immutable request and the execute phase; the v0.1 contract already forbids the Worker from widening its own task or Work state.

Clause: [post-v0.1 milestones](post-v0.1-milestones.md#planning-and-bounded-replanning); [schemas](worker-protocol.md#schemas). Suite: WS-19 (a Worker output attempting to change Work state is ignored and recorded).

## R308

Schema-invalid machine output — re-modelled. There is no Worker-internal self-repair session: a schema-invalid report is retained invalid evidence, report recovery is bounded, and any repair is a fresh Core-authorized Invocation — never an adapter-local session and never a rewrite of the recorded artifact. The retained obligation — detect invalid output, bound the handling, never touch the real artifact — is met by the invalid-evidence rules. Approved re-model in the 2026-11-08 #52 session.

Clause: [evidence validity](worker-protocol.md#evidence-validity); [adapter-local retries](worker-protocol.md#adapter-local-retries). Suite: WS-09.

Planned proof for AC5: A malformed report is retained invalid; bounded recovery re-observes; the artifact bytes are identical after every exit path.

## R315

Workspace-scope confinement is enforced, not audited — re-modelled. The source audited out-of-scope tool inputs because it could not fully trust enforcement; the target substitutes mount-namespace confinement: a read or write outside the granted binds is impossible, and preflight probes prove it. The retained obligation — confinement must not rest on the runtime's own cooperation — is met structurally.

Clause: [selected profile](worker-protocol.md#selected-profile) (filesystem); [preflight](worker-protocol.md#preflight). Suite: WS-14, WS-15.

## R317

Materializing an untrusted revision never executes repository-controlled code. The workspace's git config, hooks, and attributes are never executed by trusted code: export runs `git bundle` inside the sandbox with hooks and fsmonitor disabled, and sandbox git uses a generated config with `credential.helper=` and `protocol.allow` limited to `file`. A checkout landing on a different revision fails the base-descendant check.

Clause: [artifact handoff](worker-protocol.md#artifact-handoff) (ownership); [selected profile](worker-protocol.md#selected-profile) (credentials and git). Suite: WS-12, WS-13.

Planned proof for AC5: A repo declaring filter drivers and poisoned config materializes without executing either; a produced commit not descending from `base` is an invalid artifact.

## R328

Worker edits confined to the granted workspace. The mount namespace permits writes only in `/workspace` and `/taisan/out`; a traversal or absolute-path escape fails at the bind — denied before any write lands, not merely audited.

Clause: [selected profile](worker-protocol.md#selected-profile) (filesystem). Suite: WS-14.

Planned proof for AC5: A write to `../escape` or an absolute host path is denied by the sandbox and recorded.
