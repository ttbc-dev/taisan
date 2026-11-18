# Proposal: Read-only evidence history export for rui

Status: proposal, not an implemented API. Tracking issue: #228. Date: 2026-11-18.

The [v1 protocol reference](rui-evidence-v1.md), [generated schema](../fixtures/rui-evidence-v1/schema.json), and [fixture freeze manifest](../fixtures/rui-evidence-v1/freeze-manifest.json) contain the T1 contract for review. Owner acceptance and merge establish the freeze for rui R1.

## Purpose

rui is a local CLI that retrieves historical findings, repair attempts and verification evidence to inform the next investigation or repair. It pulls recorded history from taisan and owns its search index. This proposal adds the producer interface; it does not add a callback from ship, review or converge to rui.

The initial consumer supports two questions: which previous findings share an explicit invariant or category, and which attempts addressed a finding with what bound verification. It returns evidence for a human or an agent to assess. It does not authorize a repair, judge a finding, or mutate execution state.

## Existing boundary and proposed addition

`work-list` and `work-detail` are inspection projections. They expose Work and Execution identities, current review finding identities and evidence summaries. They are not a versioned export of every historical review round, finding body, repair-to-finding link and exact-head verification.

Add an evidence query boundary instead of making the consumer read the store, depend on internal Rust crates, reconstruct relationships from display strings, or scrape logs. The producer emits native execution facts; the consumer maps them to its domain. A Work is not an Execution, and an Invocation is not an independent logical run.

The CLI is the first transport. Reuse the existing outer JSON result envelope and exit-code classes. No HTTP server, event subscriber or rui dependency is required. An HTTP transport may be added later over the same contract.

## Operations

The following commands are proposed additions:

```sh
taisan --config CONFIG evidence export [--after CHECKPOINT | --cursor PAGE_CURSOR] --limit 100
taisan --config CONFIG evidence artifact --id ARTIFACT_ID --sha256 DIGEST --output NEW_PATH
```

Both commands read recorded data only. They must not refresh host observations, invoke a worker, change a review judgment, advance execution, or contact a remote host to manufacture missing evidence. Artifact writes are confined to the caller-selected new output file. Export may use a bounded query snapshot cache, but must not mutate authoritative Work/Execution state.

An initial export omits both tokens and returns retained history. `--after` starts a new change export after a previously completed checkpoint. `--cursor` continues a page sequence. Supplying both tokens is a usage error. The tokens are opaque and cannot be parsed by consumers.

`--limit` counts entity and link revisions together, defaults to 100 and has a hard maximum of 500. A page is at most 4 MiB including artifact descriptors and gaps. If the next indivisible record cannot fit, return `record_too_large`; do not skip it and advance the checkpoint. Artifact bytes are fetched separately.

## Snapshot and pagination

Every page has `installation_id`, `store_epoch`, `snapshot_seq` and `snapshot_id`. An export captures one upper watermark on its first page. Later pages return the same records and artifact references that were visible at that watermark, even if execution continues. A repeated cursor returns the same logical page. A new change is visible on the next export, never inserted into the current page sequence.

A continuing cursor binds the installation, epoch, lower checkpoint, upper watermark and next stable position. The producer may reconstruct from retained immutable events or materialize a snapshot. The implementation must prove that method with tests; repeated live queries without an upper bound are insufficient.

Sort revisions by `(change_seq, record_kind, source_id, revision)` so equal sequence values do not cause omissions. Do not advance the cursor by sequence alone inside a tied group. Artifacts referenced by the snapshot bind to immutable digests.

Non-final pages contain `next_cursor` and a null `checkpoint`. The final page contains a null `next_cursor` and the new `checkpoint`. An empty final page is valid. Only the final checkpoint authorizes a consumer to advance its sync position.

A snapshot expires after a documented TTL, initially at least 10 minutes. `snapshot_expired` instructs the consumer to discard the unfinished page sequence and restart from its last committed checkpoint. `checkpoint_expired` means the lower-bound history is no longer available and requires a new full retained-history import into a separate consumer generation/home. Never silently convert an expired checkpoint into a successful empty delta.

`store_epoch` changes when a restore or reset invalidates prior sequence history. Tokens from another installation or epoch return a structured error. Do not reuse an identity whose immutable contents have changed.

## Result envelope

Keep the existing [CLI result envelope](execution-loop.md#cli-contract), including `schema_version`, `status`, `command`, `work_id`, `execution_id`, `result`, `reason`, `evidence_refs`, `sources`, and `as_of`. An accepted export has an inner protocol version:

```json
{
  "protocol_version": "rui-evidence/1",
  "installation_id": "installation-example",
  "store_epoch": "epoch-example",
  "snapshot_id": "snapshot-example",
  "snapshot_seq": 120,
  "retained_from_seq": 1,
  "after_seq": null,
  "entities": [],
  "links": [],
  "artifacts": [],
  "gaps": [],
  "next_cursor": null,
  "checkpoint": "opaque-completed-checkpoint"
}
```

The example shows an empty final page, not a complete populated fixture. `snapshot_seq` must equal the outer query's `as_of`. The inner protocol version is independent of the outer CLI version. Consumers reject unknown major versions. Additive optional fields within version 1 are permitted and retained in raw receipts.

Top-level `accepted` means the query ran successfully. It does not assert that all historical evidence exists. `gaps` carries domain coverage gaps separately from query failure.

## Records and identity

Each entity or link is an immutable revision in this wrapper:

```json
{
  "source_id": "finding-example",
  "revision": "revision-example",
  "revision_seq": 1,
  "change_seq": 117,
  "body_sha256": "<64 lowercase hex characters>",
  "body_json": "<exact UTF-8 JSON text of one typed record>"
}
```

`body_sha256` hashes the exact decoded UTF-8 bytes of `body_json`; it is not a hash of reserialized JSON. Persist and return those bytes unchanged. This avoids reliance on language-specific object serialization. The parsed body repeats its `source_id` and `revision`; the consumer checks that they match the wrapper. Floating-point identity or timestamps are not required. SHA-256 is an integrity identifier, not proof that the record's assertions are true.

The full identity is `(installation_id, store_epoch, source_id, revision)`. A new revision may correct or enrich a logical entity. `revision` is opaque. Each logical source ID also has a strictly increasing positive `revision_seq`; select its current revision by that number, never lexicographically by revision text. The body repeats this number. The same source ID and revision_seq must identify exactly one revision. The same full identity with another body digest is a producer error. Emit prior revisions on a full retained-history export, and emit new revisions on change exports. Never overwrite immutable history in place.

All record bodies carry `record_kind`, `source_id`, `revision`, `revision_seq`, `work_id`, `assertion`, `source_refs`, and `operation`. Entity upserts carry `type` and a typed `payload`. Links carry a relation and endpoints. Tombstones carry a reason and identify the entity type or link relation, as defined in the [protocol reference](rui-evidence-v1.md#record-wrapper-and-body). Assertions distinguish `observed`, `source_claim`, `human_inference` and `model_interpretation`; interpretation records include author/tool/version/rationale. Each source reference identifies an artifact ID, SHA-256 and a locator. A derived projection may cite an immutable snapshot artifact of the underlying retained fact, produced before the export record that references it. Avoid a record citing an artifact containing its own source-reference digest. That artifact must be separately fetchable and hash-verified, and must not be presented as independent primary-source corroboration.

Use these entity types and fields:

| Type | Required payload and explicit unknowns |
| --- | --- |
| work | work identity, version, dispatch-time intent/requirements/constraints and their source revision; missing original snapshots are a gap |
| execution | execution identity, work identity, recorded state/outcome, optional base/result head and timestamps |
| invocation | invocation identity, execution identity, process-attempt identity and recorded state |
| review_round | round identity, execution/change identity, reviewed head, completeness and source observation identity |
| finding_occurrence | occurrence identity, source finding key, round identity, reviewed head, body when retained, disposition/status; optional explicit category/invariant references |
| repair_attempt | attempt identity, execution identity, round identity where recorded, target/result head, status and optional sequence |
| verification | check identity, command/check name, tested head, status, scope, exit code when recorded |
| outcome | identity, execution/review/publication scope, recorded state and head where applicable |
| requirement / invariant | identity, text, explicit category when recorded, and dispatch/annotation provenance |

Bodies and head values must come from retained records. Null means unknown and must not be replaced with the current head. If a body is not retained, expose the identity and gap rather than inventing text. Source finding keys are not globally unique; occurrence identity is authoritative.

An endpoint revision must resolve in the full page sequence or in the retained history preceding the supplied checkpoint. If retention removed that endpoint, include a gap naming the exact missing ID/revision and retain the link as unsupported source evidence; a consumer archives it but does not index it as a supported relation. A dangling endpoint without such a gap is a malformed export. If evidence is later recovered, emit the recovered endpoint and a new revision of affected links in a delta so consumers can materialize the relation. Page order does not determine whether a reference exists.

Use typed links for work/execution/invocation membership, round/finding membership, work requirements and constraints, repair membership, `finding_violates_invariant`, `repair_addresses_finding`, `verification_tests_repair` and recorded outcomes. A link has its own source ID/revision, relation enum, endpoint ID/revision pairs, assertion and source references. Endpoint revisions are explicit so a check cannot migrate to a corrected or unrelated head.

Every asserted relation needs its own supporting record or cited source fragment. Matching timestamps, commit order, a decision to fix, or a successful process exit do not establish repair or verification relationships. The absence of a recorded link is a gap, not permission to infer one. A failed check can test an attempt. The export must distinguish that binding from a passed result.

## Corrections, missing evidence and retention

Corrections create new revisions. Explicit retractions use a tombstone revision with reason and source reference. Omission from an incremental page never means deletion. Consumers retain prior revisions while selecting current logical versions by `revision_seq`.

A full export identifies the retained-history boundary as `retained_from_seq` and includes gaps for earlier purged history. A checkpoint outside that boundary fails explicitly. If an artifact descriptor reports an artifact as available but retrieval cannot supply the required bytes, fail artifact retrieval; do not substitute another revision or a human-readable summary.

Each gap has a code, scope identity, evidence class and reason. Initial codes include `not_recorded`, `not_yet_observed`, `retention_expired` and `source_unavailable`. This gap vocabulary is for missing domain evidence inside a successful snapshot. Store/query failure, invalid cursors and a broken snapshot are errors, not domain gaps.

The producer may complete a snapshot containing declared domain gaps. The consumer can publish it with partial coverage. Missing review evidence is never an empty clean review, and an unknown result is never success.

## Artifacts

An artifact descriptor includes opaque ID, SHA-256, byte length, media type, availability and revision identity. Never expose an arbitrary host path for the consumer to dereference. A referenced artifact may be returned on the page that first references it; repeated descriptors must be identical.

`evidence artifact` resolves only an ID in taisan's retained registry, verifies its binding and writes the exact bytes to a new caller-selected file. Refuse an existing destination. Write through a temporary file and rename only after digest validation. On error leave no completed output file. Output exactly one JSON envelope to stdout containing descriptor and byte count; no binary or artifact body goes to stdout or diagnostics.

Limit one artifact to 8 MiB in the initial contract. Return `artifact_too_large` rather than truncate. Large raw logs can be represented by retained bounded source fragments if their acquisition and locator semantics are explicit; do not silently clip bytes under an existing digest. Hash binding does not guarantee semantic support; consumers must preserve this distinction.

## Compatibility and failures

Map errors to the existing CLI exit classes; no new incompatible outer envelope is required. At least these reason codes are distinguishable: `unsupported_protocol`, `invalid_cursor`, `snapshot_expired`, `checkpoint_expired`, `installation_mismatch`, `epoch_mismatch`, `record_too_large`, `artifact_unavailable`, `artifact_too_large`, `digest_mismatch` and the envelope's `usage`/`config`/`store`/`forbidden`/`store_unavailable` failure classes.

A structured unavailable response is not an empty successful export. Diagnostics contain codes, counts and safe identifiers, not retained private bodies, credentials or host paths. Process stdout remains bounded JSON. Consumers retain their last committed checkpoint after an error.

No network credentials are added to the wire. Local CLI access follows existing installation authorization. A future remote transport needs explicit authentication and scope authorization before implementation. It is out of scope here.

## Implementation sequence

### T1: Protocol types and synthetic contract fixtures

The v1 reference and freeze manifest above identify the T1 contract inputs for consumer R1.

Define the version 1 export schema and artifact operations, including all enums and endpoint-revision rules. Add a populated synthetic fixture with multiple executions and review rounds, a failed repair, a separately verified repair, missing evidence, a correction and a tombstone. Add two-page, expired-token, unknown-version and artifact-error examples. All fixtures are invented and contain no operator paths or real repositories. Freeze the schema and fixtures before consumer task R1; producer implementation T2/T3 and consumer implementation R2/R3 can then proceed independently.

Acceptance: schema fixtures parse with the public types; record digests and artifact locators resolve; unknown required versions fail; assertion and head-binding cases are explicit. The consumer and producer use the same versioned protocol examples.

### T2: Snapshot exporter over recorded state

Expose historical records through a bounded read-only query, not the latest-view projection. Preserve ties in change sequence, capture an upper watermark and expose retention boundaries. Where the store lacks a required fact, emit a gap. Do not expand into worker instrumentation just to claim complete export.

Acceptance: writes between page requests do not change the page sequence; retrying a cursor returns the same logical records; all pre-watermark records appear exactly once; post-watermark records appear on the next delta. Expired snapshot/checkpoint, reset epoch, ties and concurrent readers are tested. A failed query never emits a completed checkpoint.

### T3: CLI and artifact retrieval

Add `evidence export` and `evidence artifact`, connect them to existing envelopes and document supported versions. Ensure paths/digests are validated at the boundary, not in consumer code that knows store layout.

Acceptance: CLI success, gaps, invalid flags, invalid cursors, unavailable artifact, digest mismatch, oversized payload, existing output file and store errors are distinguishable. Every artifact returns exact retained bytes or an explicit error. Normalized authority rows are unchanged; no worker, remote refresh or host write occurs. Existing command contracts still pass.

### T4: Consumer acceptance and operational guidance

Run rui against an isolated taisan installation through the real CLI. Confirm initial import, delta replay, update during pagination, correction, restart and declared retention gaps. Record producer/consumer revisions and protocol version privately for real source data; publish only synthetic examples and reviewed aggregate outcomes.

Acceptance: repeated sync does not duplicate data; the consumer advances its checkpoint only after commit; finding/repair/check identities and heads can be followed to their evidence; unavailable history stays visible. Fixture-only success and live compatibility are reported separately.

## Scope and review decision

This document adds a proposed contract and implementation tasks. It does not implement the API, reserve a release version, or require a change to the scheduler, Work lifecycle, review authority, worker protocol or default retention policy. T2 may identify missing store-query support; any schema or retention change must be explicit in that implementation's design and review.

The proposal can be accepted independently of rui's retrieval implementation. The producer must remain useful as a read-only export boundary without knowing graph schemas, retrieval scores or consumer-specific categories.
