# Retention and cleanup requirement mapping

Status: normative target obligations for Issue #54, derived from the complete pinned inventory in [extraction coverage](extraction-coverage.md). The category limits, pin model, eligibility matrix, scheduling, accounting, and availability states are specified in [retention policy](retention-policy.md). This is documentation only. No requirement below claims runtime implementation, executed cleanup, or live evidence.

## Reading and mapping rules

Each R heading is a precise target for the #54-owned inventory link. The scope is that requirement's retention-owned obligation — the part the retention policy, dependency pins, cleanup eligibility, or accounting contract must satisfy — not its entire source mechanism. Obligations owned by evidence gates (#51), the Worker contract (#52), configuration (#53), or the loop (#55) are cited, not duplicated.

Every mapping names the governing clause and the retention contract suite (RT) cases that prove it. Where the retained safety obligation is met by a different mechanism than the source's — protective-dependency pins in place of a personal prune service, catalog states in place of liveness probes — the substitution is recorded explicitly. Source pruning mechanisms (cron-driven gc, container quarantine verbs, directory layouts) are omitted while their preservation requirements transfer; that distinction is stated per requirement.

The proof paragraphs are planned contract scenarios. Run them through the future public Command, Query and tooling paths with literal expected results, retained metadata and reclaimed states. Structural checks supplement those scenarios; they cannot establish live behavior. This document's 18 requirement headings cover all 18 #54 inventory targets at this revision. Counts can be regenerated with the extraction checker and by counting targets whose `owner_issue` is 54 in `docs/extraction-coverage.json`.

## R15

Stop proof and uncertainty — retention slice: a workspace, out directory, or checkpoint belonging to a live or unproven writer is never reclaim-eligible. An inspection or `lsof`/metadata failure is not stop evidence; the candidate is retained with a warning observation, and no sweep outcome may be read as "writer gone".

Clause: [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix); [dependency pins](retention-policy.md#dependency-pins). Suite: RT-03, RT-06.

Planned proof for AC2: Leave a `setsid` child of a dead leader and run a sweep — the workspace stays pinned; inject a metadata-read failure on an eligible-looking workspace and verify it is retained and warned, not reclaimed.

## R17

Exact retained change and verification — retention slice: handoff refs, checkpoint bodies, and payload bodies needed to satisfy verification remain pinned while their protective dependencies stand. Removing required object bytes denies completion and preserves recovery references; reclaiming the bodies later still leaves the digest-bound references resolvable as `unavailable`, never as absent evidence.

Clause: [dependency pins](retention-policy.md#dependency-pins); [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix); [evidence availability states](retention-policy.md#evidence-availability-states). Suite: RT-04, RT-07, RT-11.

Planned proof for AC5: Remove required object bytes after a timeout and deny completion while preserving recovery references; reclaim the artifact ref after terminal + age and verify the reference resolves `unavailable` with reason, not `absent`.

## R27

Durable judgment history — retention slice: immutable rounds, occurrences, decisions, and source-availability rows are metadata and never reclaimed, compacted, or rewritten. Judgment payload bodies may move to CAS and be reclaimed under policy; every reference keeps identity and digest and resolves `unavailable` rather than appearing as an empty decided set. A corrupt or incomplete ledger body is `unavailable`/`corrupt`, not a clean slate.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [evidence availability states](retention-policy.md#evidence-availability-states). Suite: RT-01, RT-11, RT-12, RT-17.

Planned proof for AC5: Reclaim an old judgment's payload body after terminal + age and restart — rounds/decisions remain queryable, the body reference reports `unavailable`, and a corrupted snapshot never reads as an empty decision set.

## R28

Finding relations across attempts — retention slice: occurrence identity and predecessor relations are durable metadata; cleanup cannot collapse, merge, or erase them. Reclaiming a finding's body preserves the occurrence identity, its provenance, and its `unavailable` body state — nonappearance in a partial review still proves nothing after the body is gone.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [dependency pins](retention-policy.md#dependency-pins). Suite: RT-01, RT-11.

Planned proof for AC5: Reclaim finding bodies across two review rounds and verify occurrence identities and predecessor links remain distinct and queryable with `unavailable` bodies.

## R30

Read-only reconciliation — retention slice: bodies required to reconcile an unresolved host effect (payload bodies, handoff refs, receipts' referenced outputs) are pinned for the effect's whole unresolved lifetime. Reconciliation reads never delete, and reclaiming other bodies can never make an unknown effect look absent — the saved intent and its references are metadata.

Clause: [dependency pins](retention-policy.md#dependency-pins); [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix). Suite: RT-04, RT-11.

Planned proof for AC5: Hold an unresolved host effect across sweeps — reconciliation inputs stay present; expire the effect to human action and confirm references remain while bodies can only leave through the eligibility gate, not ambiguity.

## R32

Recorded bounded recovery — retention slice: recovery counters, attempt charges, class decisions, and equivalent-failure history are durable metadata that no cleanup may erase, reset, or summarize away. A recoverable missing artifact keeps its pin until the bound is exhausted; bound exhaustion ends recovery, not by deletion but by lifecycle decision.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix). Suite: RT-01, RT-07.

Planned proof for AC2: Run sweeps over a Work mid-recovery — counters and class decisions are byte-identical; after bound exhaustion the pin discharges by lifecycle event, and only then does age begin.

## R34

Foreign and unknown writers — retention slice: a body not attributable to managed identity is never sweep-eligible. It is evidence of a possible unknown writer, retained until an explicit, auditable operator repair Command removes it. Quarantined or foreign evidence is preserved with provenance through retention, never laundered into "absent".

Clause: [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix) (foreign/unattributable row). Suite: RT-06.

Planned proof for AC5: Seed a foreign directory beside managed bodies, sweep, and verify only managed eligibles are reclaimed; the foreign body is retained, reported, and requires an operator Command to remove.

## R38

Secret-safe evidence — retention slice: reclaiming a body never exposes anything: CAS and catalog entries carry only digests, kinds, sizes, and reasons; accounting output is bounded and sanitized; bodies that ever held credentials were never stored — credential resolution happens at trusted boundaries and only opaque references reach retained state. A reclaimed body leaks nothing because nothing secret existed in retained form.

Clause: [accounting, pressure, and repair paths](retention-policy.md#accounting-pressure-and-repair-paths); [evidence availability states](retention-policy.md#evidence-availability-states). Suite: RT-11, RT-21.

Planned proof for AC5: Seed a secret-looking token in a body pre-store (rejected at ingestion), inspect catalog/report output for leakage, and verify `unavailable` reasons carry no body content.

## R41

Configurable retention with safety-preserving reclamation — the core requirement. Every category has configurable age and byte limits with documented defaults; reclamation preserves all pinned dependencies, counters, and unresolved effects; unknown writers/effects are never reclaimed into absence; results are recorded and reclaimed bodies surface as `unavailable`. Source mechanisms — the ops repo's personal cleanup service and prune helpers — are omitted; their safe-retention and observable-partial-outcome properties transfer to the bounded sweep and the catalog.

Clause: the whole of [retention policy](retention-policy.md) — [categories/limits](retention-policy.md#configuration), [pins](retention-policy.md#dependency-pins), [eligibility](retention-policy.md#cleanup-eligibility-and-failure-matrix), [scheduling](retention-policy.md#scheduling-and-ownership). Suite: RT-01–RT-24.

Planned proof for AC1: Exercise old/young bodies across all six categories, storage pressure, active/paused/recovering/unknown-effect pins, interrupted cleanup, and missing bodies — every rule above holds with observable partial results.

## R63

Automation-path retirement classification is evidence-bound and never destructive on audit alone — retention slice: a `legacy candidate` classification is metadata about disuse evidence; it produces no removal effect. Removal requires the explicit eligibility gate — positive terminal evidence plus discharged pins — never the audit label by itself.

Clause: [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix) (retired-legacy row). Suite: RT-19.

Planned proof for AC5: Mark a store region "legacy candidate" in an audit report and sweep — nothing is removed; run the explicit eligibility path on a truly terminal region and record the distinct decision.

## R84

Usage accounting is a read-only derived report over immutable versioned observations — retention slice: `retention report` derives entirely from `body_catalog` plus dependency rows and filesystem stat; it mutates nothing, charges nothing twice, and reports `unknown` rather than `0` when a region cannot be read. Immutable observations (the catalog rows) are the only inputs.

Clause: [accounting, pressure, and repair paths](retention-policy.md#accounting-pressure-and-repair-paths). Suite: RT-10, RT-21.

Planned proof for AC5: Recompute a report twice — identical output with zero writes; make a region unreadable and verify `unknown` counts appear named, never silently zero.

## R85

Bounded telemetry ingestion — retention slice: importer checkpoints, journal heads, and queue-drop counters are durable metadata and never reclaimed; the sweep creates no backpressure on ingestion — it defers when the store is busy and its deletions run outside the write path. A loss-counted drop stays visible in accounting after any sweep.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [scheduling and ownership](retention-policy.md#scheduling-and-ownership). Suite: RT-01, RT-18.

Planned proof for AC5: Sweep during active ingestion — checkpoints and drop counters are untouched, ingestion resumes from its checkpoint, and the sweep's candidates were filtered before any deletion.

## R111

Claim release re-verifies the record generation under lock — retention slice: an expired external claim releases through a metadata transaction that rechecks the record generation before commit; a claim refreshed mid-release wins. Release is not stop evidence and never authorizes body deletion — the workspace pin comes from Execution state, not the claim. A terminal unlinked provisional record is archived, never deleted by the sweep.

Clause: [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix) (expired claim row). Suite: RT-05, RT-20.

Planned proof for AC5: Race a claim TTL renewal against its release commit — generation recheck fails and the claim stands; verify the workspace stays pinned by Execution state through and after the release.

## R169

Group admission and bounded triage retries — retention slice: preserved timed-out WIP is evidence, not disposable output — workspaces/out dirs of bounded-retry Executions stay pinned while retry remains eligible. Investigation/verdict records and retry counters are durable metadata; cleanup cannot reset a bounded retry or dispose of WIP a retry may consume.

Clause: [dependency pins](retention-policy.md#dependency-pins) (retry-eligible row); [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies). Suite: RT-01, RT-03.

Planned proof for AC5: Hold a failed-but-retryable Execution across sweeps — workspace pinned, counter intact; settle the Work terminally and verify eligibility only begins at terminal + age.

## R244

Disposable-checkout gc — retention slice: a managed workspace is reclaimable only after its owning Work is terminal (remote state reconciled) plus the age window; audit/evidence bodies are not disposable checkouts and are never touched by workspace reclamation. The source's legacy directory layout and gc implementation are omitted; the terminal-state-only invariant transfers.

Clause: [cleanup eligibility and failure matrix](retention-policy.md#cleanup-eligibility-and-failure-matrix) (terminal Work row); [retention categories](retention-policy.md#configuration). Suite: RT-03, RT-04.

Planned proof for AC5: Attempt workspace reclaim on a nonterminal Work and on a terminal Work inside the age window — both retained; reclaim after the window and verify audit bodies elsewhere in the catalog are untouched.

## R256

Model-source pinning over persisted evidence — retention slice: the recorded model identity and evidence it produced are durable metadata; CAS payload bodies that carry recorded model/source identity are pinned while a live gate depends on them and reclaimable only afterward — identity and digest always remain. Re-checking reported model over persisted evidence works against metadata even when bodies are `unavailable`.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [evidence availability states](retention-policy.md#evidence-availability-states). Suite: RT-01, RT-11, RT-16.

Planned proof for AC5: Reclaim a model-tagged payload body after terminal + age — model identity and digest stay queryable, the body reports `unavailable`, and evidence re-checks still classify validity rather than treating absence as match.

## R257

Ordered publication gates and refusal records — retention slice: typed refusal records, posted-evidence receipts, and partial-effect receipts are durable metadata retained permanently and bounded in size by their schemas, not by deletion. A later failure never erases an applied receipt; a sweep cannot dispose of refusal history needed for audit.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [accounting, pressure, and repair paths](retention-policy.md#accounting-pressure-and-repair-paths). Suite: RT-01, RT-11.

Planned proof for AC5: Sweep over a Work with a valid push receipt and a later failed create — both records persist verbatim; only body-class state is eligible for reclaim.

## R296

Validated byte-digested pricing snapshot — retention slice: a pricing snapshot is a CAS body whose digest identity is permanent; estimates derive only from the validated snapshot, and an `unresolvable` snapshot (absent, corrupt, or reclaimed) yields an `unavailable` estimate with a named reason — mapping exactly to the catalog's `unavailable` states rather than a guess.

Clause: [durable metadata versus reclaimable bodies](retention-policy.md#durable-metadata-versus-reclaimable-bodies); [evidence availability states](retention-policy.md#evidence-availability-states). Suite: RT-11, RT-12.

Planned proof for AC5: Reclaim or corrupt a pricing snapshot, then request an estimate — result is `unavailable` with the reclaim/corrupt reason; a valid snapshot still yields a computed estimate bound to its digest.
