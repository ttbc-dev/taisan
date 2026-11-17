# rui evidence protocol v1

T1 defines the wire contract and synthetic examples for issue #228. The
[Rust types](../crates/workloop-evidence-protocol/src/lib.rs),
[JSON Schema](../fixtures/rui-evidence-v1/schema.json), and
[freeze manifest](../fixtures/rui-evidence-v1/freeze-manifest.json) identify
the contract at this revision. Owner acceptance and merge establish the freeze
for consumer R1. T2, T3, and T4 cover the live exporter, CLI, and interoperability.

The schema root groups named message definitions for lookup. Validate a message
against its definition. Parse `body_json` separately against `RecordBody`
after checking its digest. The [semantic validator](../crates/workloop-evidence-protocol/src/validate.rs)
checks rules that span fields, messages, and artifact bytes.

## Encoding and limits

Use UTF-8 JSON. Except where documented, fields are required. A nullable field
accepts null or omission and means unknown or inapplicable. Producers emit
null for absent fields except envelope `sources` and `as_of`, which they omit.
Arrays are required even when empty. Send integers without fractional parts.
Treat sequence numbers as unsigned 64-bit integers without rounding.

Opaque identifiers, tokens, and bounded labels are nonblank and at most 1,024
UTF-8 bytes. Treat tokens and identifiers as case sensitive. SHA-256 values
contain exactly 64 lowercase hexadecimal digits. The raw inner export page
limit is 4 MiB, including embedded JSON text, gaps, and descriptors. Each page
contains at most 500 combined record revisions, 2,000 descriptors, and 2,000
gaps. At the CLI boundary, use `decode_export_envelope` to check the original
`result` bytes before decoding the page. For standalone pages, use
`decode_page`. Semantic validation of an already decoded page checks count
limits and cannot recover the original wire length. Each record contains up to 32 source references and 500 intrinsic
revision references. Free text in payloads fits within the page limit. An
available artifact contains at most 8 MiB.

Readers reject unknown outer versions and inner `protocol_version` values,
unknown required tags and enum values, and malformed fields. Readers may
ignore additive optional fields of known types within v1. Preserve original
receipts and artifact bytes at the consumer boundary so that projection
through typed objects does not discard unknown fields or change hashed text.

## Envelope and requests

The CLI reuses [result envelope v1](execution-loop.md#cli-contract).
`schema_version` is the string `"1"`. Queries use `accepted` and `error`.
`command` contains the query `kind` (`evidence_export` or `evidence_artifact`),
`actor`, nullable `id`, and null `idempotency_key`. Envelope `work_id` and
`execution_id` are null. `evidence_refs` holds up to 16 safe strings of up to
240 characters. `sources` lists up to 2,000 source observations on accepted exports.
Each source has a `source` label, `availability` (`known`, `unknown`,
`unavailable`) and nullable `observed_unix_seconds`. Export `as_of` equals
`snapshot_seq`. Artifact receipts and errors omit both fields. Acceptance
carries a typed `result` and null `reason`. An error carries null `result`
and a `reason` with a code and a nonblank message of at most 240 characters.

An export request sets `protocol_version` to `rui-evidence/1`. Its positive
`limit` counts entity and link revisions together. The CLI defaults to 100.
Set at most one of `after` (a committed checkpoint) and `cursor` (a page
continuation). Omit both for a full retained-history export.

## Export page and snapshot

| Field | Meaning |
| --- | --- |
| `protocol_version` | `rui-evidence/1` |
| `installation_id`, `store_epoch` | Sequence and identity namespace. Reset the epoch when prior sequence history is invalidated. |
| `snapshot_id` | Identity of one immutable, possibly paginated view. |
| `snapshot_seq` | Inclusive upper watermark. |
| `retained_from_seq` | Inclusive lower retention boundary. A full export with a boundary greater than 1 includes a history gap. |
| `after_seq` | Exclusive checkpoint watermark for a delta. Null for a full export. |
| `entities`, `links` | Revision wrappers. Each array follows the relative order of its records in the merged stream. |
| `artifacts`, `gaps` | Descriptors and declared coverage gaps. Repeated descriptors are identical. |
| `next_cursor`, `checkpoint` | A continuing page has records and only a cursor. A final page has only a checkpoint and may be empty. |

Merge records by `(change_seq, record_kind, source_id, revision)` with
`entity` before `link` and strings in UTF-8 byte order. A tie may span pages.
All pages share the same identity, retention boundary, and watermarks. Each
continuation request uses the previous page's cursor. Only advance the consumer
sync position after committing the final page. A delta resolves references
against its pages and validated history for its supplied lower checkpoint.
Each new export sequence uses a fresh snapshot ID. A checkpoint may recur
only at the same watermark. These rules apply across all validated prior
snapshots in the chain.

## Record wrapper and body

The wrapper contains `source_id`, `revision`, positive `revision_seq`,
`change_seq` within the page's watermarks, `body_sha256`, and `body_json`.
Hash the exact UTF-8 bytes of the decoded string, including whitespace. The
body repeats `source_id`, `revision`, and `revision_seq`. It adds
`record_kind` (`entity` or `link`), `work_id`, `assertion`,
`source_refs`, and the discriminator `operation`.

The installation, epoch, source ID, and revision identify immutable bytes.
Revisions of one logical source keep its kind and type and increase
`revision_seq` in stream order. Select the current revision by that sequence.
Keep earlier revisions for explicit references. Strings in revision
references are opaque. A revision reference consists of `source_id` and
`revision` in the snapshot namespace.

| `operation` | Fields specific to the operation |
| --- | --- |
| `entity` | `type` and its typed `payload` |
| `link` | `relation`, `from`, `to` |
| `entity_tombstone` | `entity_type`, `reason` |
| `link_tombstone` | `relation`, `from`, `to`, `reason` |

Tombstones retract a source under a new revision. They carry a nonblank
`reason` and source evidence. Active links also carry source evidence. Omission from a delta does not retract a source.

`assertion.kind` is `observed` (recorded fact), `source_claim` (claim made
by a source), `human_inference`, or `model_interpretation`. The last two
require `author`, `tool`, `version`, and `rationale`.

### Entity payloads

`?` marks a nullable field. Named references in this table are exact
revision references. IDs within payloads name the recorded domain entities.

| `type` | `payload` |
| --- | --- |
| `work` | `work_id` repeats the body's work ID. `version` is positive. `intent?` is dispatch text. `requirements` and `constraints` list requirement and invariant revisions. `dispatch_source?` cites the original dispatch revision. |
| `execution` | `execution_id`, `work` reference, `state`, `outcome?` reference, `base_head?`, `result_head?`, `started_at?`, `finished_at?`. Times are retained timestamp strings. |
| `invocation` | `invocation_id`, `execution` reference, `process_attempt_id`, `state`. |
| `review_round` | `round_id`, `execution` reference, `change_id`, `reviewed_head?`, `completeness`, `observation_id?`. |
| `finding_occurrence` | `occurrence_id`, source-local `source_finding_key`, `round` reference, `reviewed_head?`, `body?` text, `disposition`, `category?` label, `invariants` references. |
| `repair_attempt` | `attempt_id`, `execution` reference, `round?` reference, `target_head?`, `result_head?`, `state`, positive `sequence?` within the recorded attempt series. |
| `verification` | `check_id`, check `name`, `tested_head?`, `state`, `scope`, signed 32-bit `exit_code?`. |
| `outcome` | `outcome_id`, `execution` reference, `scope`, `state`, `head?`. |
| `requirement` | `requirement_id`, `text?`, `category?`, `provenance`, `source_revision?` reference. |
| `invariant` | `invariant_id`, `text?`, `category?`, `provenance`, `source_revision?` reference. |

`state` is `pending`, `running`, `passed`, `failed`, `cancelled`, or
`unknown`. `completeness` is `complete`, `partial`, or `unknown`.
`disposition` is `open`, `accepted`, `dismissed`, `resolved`, or
`unknown`. `provenance` is `dispatch` or `annotation`. Verification
`scope` is `targeted`, `workspace`, `integration`, or `unknown`.
Outcome `scope` is `execution`, `review`, or `publication`. Head strings
name retained revisions. Null conveys an unknown head.

### Relations and gaps

| `relation` | From | To |
| --- | --- | --- |
| `work_has_execution` | Work | Execution |
| `execution_has_invocation` | Execution | Invocation |
| `execution_has_round` | Execution | Review round |
| `round_has_finding` | Review round | Finding occurrence |
| `work_has_requirement` | Work | Requirement |
| `work_has_invariant` | Work | Invariant |
| `execution_has_repair` | Execution | Repair attempt |
| `round_has_repair` | Review round | Repair attempt |
| `finding_violates_invariant` | Finding occurrence | Invariant |
| `repair_addresses_finding` | Repair attempt | Finding occurrence |
| `verification_tests_repair` | Verification | Repair attempt |
| `execution_has_outcome` | Execution | Outcome |

Endpoints share the body's `work_id`. Typed references resolve to entity
revisions of the stated type. An active `verification_tests_repair` with
both endpoints present requires non-null `tested_head` equal to
`result_head`, regardless of whether the check passed.

A gap contains `code` (`not_recorded`, `not_yet_observed`,
`retention_expired`, `source_unavailable`), `evidence_class` (`body`,
`relation`, `artifact`, `history`, `intent`), `reason`, and a typed
`scope`. The scope's `kind` is `revision` with `record`, `artifact`
with `artifact_id` and `sha256`, or `history` with `before_seq`.
The history scope uses `retention_expired` and class `history`. It names
the retention boundary. Each missing endpoint, including an intrinsic
payload reference, requires a `relation` gap for that exact revision.
Archive links with missing endpoints as unsupported relations. When an
exact endpoint arrives, recheck the old link or intrinsic reference for type
and head consistency. The archived link remains unsupported. A new link
revision with valid endpoints establishes support.

A finding with null or omitted `body` requires an exact revision gap of
class `body`. A work record with null or omitted `intent` requires one
of class `intent`. Unknown heads and outcomes do not require those gaps.

## Artifacts and locators

An artifact descriptor has `artifact_id`, `identity`, `sha256`,
`byte_length`, `media_type`, and `availability` (`available`,
`unavailable`, or `retention_expired`). The identity has
`installation_id`, `store_epoch`, `source_id`, `revision`, and positive
`revision_seq`. It identifies the artifact's immutable source revision
in the page namespace. That source may be a retained observation bundle
rather than an exported record. Artifacts and exported records share the
revision-to-sequence mapping in both directions. The artifact digest hashes
source bytes independently of the projected record body digest. Multiple artifact IDs may name the same
revision if its revision sequence, digest, byte length, and media type
agree. Availability is reported per artifact ID. Within one source, a
revision sequence identifies exactly one revision. These bindings hold across
pages and validated preceding history. Repeat a descriptor identically within
one snapshot. Across snapshots, the same ID may declare a new availability
with its immutable binding unchanged.

Each `source_refs` entry has `artifact_id`, `sha256`, and a `locator`.
A `json_pointer` locator contains an RFC 6901 `pointer` into the actual
JSON bytes after parsing. An empty pointer selects the document. The media
type is `application/json` or ends in `+json`. A `byte_range` locator
contains zero-based inclusive `start` and exclusive `end`, with
`start < end <= byte_length`. A referenced unavailable artifact requires
an exact artifact gap. Verify available bytes and locators before relying
on them. A digest establishes byte identity.

An artifact request contains `protocol_version`, `artifact_id`, and
`sha256`. The CLI additionally accepts a new local output path. An accepted
artifact result contains `protocol_version`, `descriptor`, and
`bytes_written` equal to the descriptor length. The bytes go to the
output file. Validate a success receipt against the exported descriptor,
including its identity, ID, digest, byte length, and media type. Both the
exported and returned descriptors must be available. The command verifies the bound digest before publishing it.

## Query failures

| Reason code | Exit class | Meaning |
| --- | --- | --- |
| `unsupported_protocol`, `invalid_request`, `invalid_cursor` | 2 | Unsupported protocol, invalid arguments, or invalid token. |
| `installation_mismatch`, `epoch_mismatch` | 2 | Token belongs to another installation or history. |
| `snapshot_expired` | 5 | Discard the unfinished sequence and restart from the last committed checkpoint. |
| `checkpoint_expired` | 5 | Reimport full retained history into a new consumer generation. |
| `record_too_large` | 5 | The next indivisible record cannot fit in a page. |
| `artifact_unavailable`, `artifact_too_large`, `digest_mismatch` | 5 | Retrieval cannot supply the requested exact bytes. |
| `config_error`, `store_error` | 3, 4 | Configuration or store failure, respectively. |

Semantic validation errors in the Rust crate distinguish invalid version,
JSON, request, limits, tokens, metadata, watermark, ordering, revision,
body digest and identity, endpoints, heads, artifact failures, and locators.
These errors describe validation of received data. Query reason codes describe
producer failure.

## Reproduce the freeze

Run from the repository root:

```sh
cargo run -q -p workloop-evidence-protocol --example schema -j 1 > fixtures/rui-evidence-v1/schema.json
python3 fixtures/rui-evidence-v1/generate.py
cargo test -p workloop-evidence-protocol -j 1
```

The manifest hashes the exact schema and example files and excludes itself.
The two full pages share a change sequence at their boundary. The delta
depends on prior revisions. The tests also verify byte length, SHA-256,
and every source locator against the included synthetic artifact. These
examples check the contract. Snapshot isolation under concurrent writes and
live CLI compatibility are T2 through T4 acceptance checks.
