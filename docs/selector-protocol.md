# Stateless external Selector protocol

Status: normative design for #200, protocol version `1` for v0.2. This document specifies a future external command boundary. It does not implement a process launcher, change installation configuration, or authorize live execution. The existing snapshot-based reference Selector remains available.

The [JSON Schema](selector-protocol.schema.json) defines the complete structural request and response shapes. The byte limits, cross-field rules, identity calculation, and admission rules below are equally normative. A schema pass alone is insufficient. This contract reuses [adapter boundaries](adapter-boundaries.md), [identity and admission](identity-storage.md), and the implemented `Proposal` and Proposal-to-submit translation in `workloop-selector`.

## Authority and invocation

The installation selects one adapter by an explicit argv vector, for example `["public-issue-selector", "select"]`. taisan launches that vector directly, without shell expansion, once per request. The adapter reads one request from stdin through EOF and writes one response to stdout through EOF. There is no daemon session, streaming candidate delivery, cursor acknowledgement, or follow-up RPC. A later request supplies a complete new scope and exclusion snapshot. Adapter caches cannot replace that snapshot or require a previous invocation.

The adapter owns source collection, selection, ranking, and grouping. It fetches its own source facts within the supplied repositories. It resolves its own source-read authentication through adapter configuration. Requests contain stable scoped identities, not source bodies, tokens, credential references, or a store handle. The launcher gives the adapter no Worker authority, installation-store access, publication credential, or inherited publication-secret environment. Source-read credentials grant no publication authority. These are launcher requirements, not a claim that JSON parsing isolates arbitrary programs.

taisan owns request construction, validation, admission, the queue, execution, and Outcome. A validated candidate goes directly through normal submit admission under the configured installation actor. No second human approval is required between validation and submit. This does not bypass existing source-freshness, capability, execution, or Outcome gates. An unavailable Selector does not stop manual submit or existing Work.

## Framing, versions, and bounds

Input and output are UTF-8 JSON objects without a BOM. Leading and trailing JSON whitespace are allowed and count toward byte limits. Duplicate object keys, invalid UTF-8, trailing non-whitespace, multiple JSON values, unknown fields, and unknown enum values are contract violations. Every field in the schema's objects is required, including nullable fields. JSON numbers in integer fields must use integer notation without a fraction or exponent.

Only `protocol_version: "1"` and the exact capability array `["grouped_sources_v1"]` are supported. The capability permits both singleton and grouped sources. There is no implicit downgrade, partial capability match, or ignored extension. Both peers reject other versions or capabilities. New fields, changed semantics, or larger bounds require a new protocol version. The adapter's positive integer `selector_version` versions selection semantics independently of the wire protocol. The launcher binds the expected adapter ID and selector version in configuration and in the request. Every candidate must match them and `policy_revision` exactly.

All byte limits count UTF-8 bytes. String limits apply after JSON unescaping. Wire limits include JSON syntax and escaping. Both limits apply when their measurements differ. JSON Schema `maxLength` constrains characters; the receiver must additionally enforce these byte limits.

| Value | Inclusive bound |
| --- | --- |
| Serialized stdin request | 1,048,576 bytes |
| Serialized stdout response, including failures | 4,194,304 bytes |
| stderr for the entire invocation | 16,384 bytes |
| JSON nesting depth, root object at depth 1 | 16 containers |
| Configured argv | 1–64 nonempty strings, 4,096 bytes each, 16,384 bytes total |
| Repositories | 1–64 unique scoped repositories |
| Excluded source issues | 0–4,096 unique scoped issues |
| `max_candidates` | 1–32 |
| Returned candidates | 0–`max_candidates` |
| Sources per candidate, also called group members | 1–32 |
| Sources across the complete response | 0–1,024 |
| Opaque identity, source revision, or content digest string | 1–512 bytes |
| Adapter ID | 1–64 ASCII bytes matching `[a-z][a-z0-9-]*` |
| Proposal key | Exactly 64 lowercase hexadecimal bytes |
| Task intent | 1–16,384 bytes, with at least one non-whitespace character |
| Task constraints | 0–64 distinct strings, 1–4,096 bytes each |
| Verification argv | 0–64 nonempty strings, 1–4,096 bytes each, 16,384 bytes total |
| Required permissions | 0–32 distinct strings, 1–128 bytes each |
| Provenance cursor | `null` or 1–1,024 bytes |
| Failure message | 1–256 bytes |
| Selection deadline | Future absolute Unix milliseconds, at most 300,000 ms after launcher's request construction time |

Strings contain no Unicode control characters U+0000–U+001F or U+007F–U+009F. Identity comparison uses exact string values, with no Unicode normalization, case folding, path parsing, or display-name substitution. Configured argv also excludes NUL and control characters. Integers use the schema's exact ranges: `policy_revision` and display numbers fit unsigned 64-bit values, `selector_version` fits unsigned 32-bit, and time values fit positive signed 64-bit milliseconds or nonnegative signed 64-bit seconds as appropriate. A display number is positive or `null`.

The launcher validates counts, strings, and encoded stdin length before spawning. An oversized repository list, exclusion snapshot, or input returns local `input_limit` without invoking the command or submitting anything. The launcher must not omit exclusions, split them across scans, truncate members, or broaden repository scope to fit. An adapter that cannot produce a complete group within the member bound returns `member_limit`, rather than a partial group. Reaching `max_candidates` may leave other independent candidates for later requests, but cannot split a selected group.

The adapter must finish source collection and output before `deadline_unix_ms`. The launcher enforces the remaining duration with a monotonic timer and terminates the command on expiry. Timeout, signal termination, nonzero exit, incomplete output, and an exceeded output or stderr limit invalidate the entire invocation. The launcher stops reading at the limit plus one detection byte and terminates the process, rather than buffering unbounded output. No candidate is submitted before normal exit and EOF on both output streams.

## Request schema

The request object is `#/$defs/request` in the schema. [The complete request example](examples/selector-protocol/request.json) uses generic public adapter and source identities.

| Field | Meaning |
| --- | --- |
| `protocol_version`, `capabilities` | Exact wire version and supported capability set above |
| `request_id` | Caller-chosen bounded correlation ID, echoed in the response, never part of proposal identity |
| `selector`, `selector_version` | Independently configured adapter identity and selection-semantics version |
| `policy_revision` | Revision of the adapter selection policy and supplied task defaults. Any semantic policy change increments this revision |
| `repositories` | Complete allowed `RepositoryRef` array, each `{ "host": ..., "repository": ... }` |
| `excluded_sources` | Complete snapshot of source issues owned by unfinished Work within the allowed repositories, including paused Work and uncertain ownership |
| `max_candidates` | Upper bound on complete candidates returned, not the number of source issues |
| `deadline_unix_ms` | Absolute deadline for this invocation |
| `task_constraints` | Constraints required on every returned task |
| `verification_argv` | Exact verification command required on every returned task, or an empty array for none |
| `required_permissions` | Exact permission declaration required on every returned candidate |

An issue reference is `{ "repository": { "host": ..., "repository": ... }, "issue": ..., "display_number": ... }`. Repository identity is the host-instance ID plus the stable opaque repository ID. Issue identity adds the repository-scoped opaque issue ID. Display numbers are metadata and never change identity. Exclusions match the scoped issue identity, or a matching non-null display number within that same scoped repository, preserving the existing reference Selector's exclusion behavior. Duplicate scoped identities are invalid even if display metadata differs. Exclusions outside `repositories` are invalid input.

The launcher supplies no source snapshot. Adapter source-read configuration maps scoped repositories to their endpoints without changing their identities. It must not scan other repositories as an implicit fallback. A required source read that fails yields `source_unavailable` or `authentication_failed`, not an empty successful scan. `policy_revision` covers adapter-specific selection policy resolved by the adapter; there is no opaque policy body for Core to interpret.

## Response schema and task mapping

A successful response is `#/$defs/success`, with `protocol_version`, `capabilities`, the echoed `request_id`, `status: "ok"`, and `candidates`. [The grouped response](examples/selector-protocol/grouped-response.json) contains one candidate with two members. [The empty response](examples/selector-protocol/empty-response.json) is a successful scan with no eligible candidates.

Each candidate is the existing Proposal shape, `#/$defs/proposal`:

| Field | Meaning and validation |
| --- | --- |
| `key` | Recomputed semantic digest described below |
| `selector`, `selector_version`, `policy_revision` | Must equal the request's values |
| `sources` | Complete nonempty group of `SourceBinding` objects. Each has an `issue`, its exact source `revision`, and `content_digest` |
| `task.intent` | One explicit intent for the whole Work, supplied by the adapter |
| `task.constraints` | Sorted distinct constraints, including every request `task_constraints` entry. The adapter may add constraints |
| `task.verification_argv` | Must equal the request's ordered argv exactly |
| `required_permissions` | Sorted distinct declaration, equal to the request's permission set. This is provenance, not a grant |
| `provenance.snapshot_cursor` | Optional bounded audit metadata, explicitly `null` when absent. Never acknowledged to the source |
| `provenance.fetched_unix_seconds` | Optional source-fetch time, explicitly `null` when absent. Never a substitute for authoritative freshness checks |

Candidates retain adapter ranking order. taisan neither sorts candidates nor reconstructs grouping. Each source must belong to a requested repository and must not match an exclusion. Group members must have distinct scoped issue identities and be sorted lexicographically by UTF-8 bytes of `(host, repository, issue)`. A source issue cannot appear in two candidates in the same response. Contradictory revisions, duplicate members with different display numbers, and duplicate candidate identities invalidate the whole response.

A group is exactly the candidate's `sources` array. There is no separate group key, member count, primary issue, per-member task, or dependency graph to reconcile. All members bind the same task. The ordered source array is also the task-context order when taisan attaches source context. `intent` remains first, constraints keep their declared order, and verification arguments keep their argv order. Any source context is attached in member order without parsing issue bodies or rewriting the task through an LLM. Cross-repository groups may be proposed only within the explicit request scope; normal installation capability and publication-binding admission rules still apply.

One candidate becomes one submit command and, if accepted, one Work with its first queued Execution. Members never become separate executions. Later stage transitions and retries remain the existing Core policy.

The Proposal-to-submit translation stays unchanged:

- `task.intent` becomes submit `intent` verbatim.
- Every source becomes a `SourceSnapshot`, retaining `issue`, `revision` as `source_revision`, and `content_digest` verbatim.
- `task.constraints` becomes submit `constraints` verbatim.
- An empty `task.verification_argv` becomes no verification commands. A nonempty argv becomes exactly one command in the submit command's list.
- Origin is `selector`. The installation supplies the actor. Replay key is `proposal/<selector>:<key>`.
- `required_permissions` participates in proposal identity and audit provenance. It is not an added submit field or a permission granted to Work. Core checks capability coverage.

## Proposal identity and replay

The adapter ID identifies the selection implementation independently of a source host or Worker. Different independent adapters use different IDs, even when proposing the same task. Replacing a binary with the same selection semantics may keep the ID and selector version. Changing selection semantics increments `selector_version`. Changing only wire representation does not redefine the proposal identity.

The key preserves the existing `proposal-v1` digest algorithm. Construct this semantic JSON object, then hash `UTF8("proposal-v1:")` followed by its canonical bytes with SHA-256:

```json
{
	"schema": "proposal-v1",
	"selector": "public-issue-selector",
	"selector_version": 1,
	"policy_revision": 7,
	"sources": [
		{
			"host": "public-host",
			"repository": "repo-17",
			"issue": "issue-21",
			"revision": "revision-3",
			"content_digest": "content-21"
		}
	],
	"task": {
		"intent": "Document the parser.",
		"constraints": ["Keep the public API unchanged."],
		"verification_argv": ["cargo", "test", "-p", "example-library"]
	},
	"required_permissions": ["workspace_write"]
}
```

This snippet illustrates the semantic payload for a singleton; the complete grouped example has a digest over both members. Canonical JSON uses recursively sorted object keys, compact separators, UTF-8 strings serialized as in `workloop_core::domain::canonical_json`, and no insignificant whitespace. Arrays retain order. Sources, constraints, and permissions use the sorted orders specified above. Constraints and permissions sort by UTF-8 bytes. The launcher recomputes the key, rather than trusting the adapter's claim.

Request IDs, deadlines, candidate rank, fetch timestamps, cursors, display numbers, and mutable running labels are excluded from identity. Different source content or revisions, task fields, selector semantics, or policy revisions produce a different key. Reordering candidates does not. Exact replay uses the saved admission decision, including after terminal release. This protocol does not automatically reprocess an unchanged terminal proposal or transfer running Work to another adapter.

## Whole-response validation and admission

Before the first submit, the launcher completes all of these checks:

1. Validate the request and establish the exact scope, identity, deadline, and limits.
2. Collect bounded output and require successful normal process exit before the deadline.
3. Parse the complete response and validate its structural schema, version, capability, and request correlation.
4. Validate every candidate's task, member consistency, scope, exclusions, bounds, request-policy agreement, semantic key, and uniqueness across the complete response.
5. Only after every candidate passes, translate candidates to submit in their returned order.

Any violation in steps 1–4 rejects the whole invocation with zero Work created, even when earlier candidates are individually valid. Failure responses also submit nothing. No best-effort adoption, truncation to `max_candidates`, unsupported-field removal, or candidate repair is allowed.

Admission in step 5 is per candidate, not an atomic batch. Core rechecks source freshness, scoped ownership overlap, capability coverage, and replay through the same path as manual submit. An ordinary rejection such as a newly acquired source overlap produces a recorded per-candidate decision and permits later candidates to proceed. A source already present in the request's exclusions is instead a response contract violation. A source claimed after request construction is an admission race, not malformed output.

Store faults and uncertain command outcomes are not ordinary admission rejections. Processing stops and retains the decided and interrupted replay keys using the existing batch-recovery semantics. Earlier accepted Work remains recorded. Retrying exact proposals recovers their saved decisions without duplicate Work. No response can authorize dispatch, settle an Outcome, or change retry or review policy.

## Failures and diagnostics

A failure response is `#/$defs/failure`. It has version `1`, an echoed `request_id` when safely parsed, otherwise `null`, `status: "error"`, and `error` with `code` and `message`. [The failure example](examples/selector-protocol/failure-response.json) reports an unavailable source. A recognized failure response exits with code 0, meaning that transport completed, not that selection succeeded. A nonzero exit is a transport failure even if stdout resembles a successful response.

| Failure code | Meaning |
| --- | --- |
| `invalid_request` | Invalid request shape, scope, identity, or task defaults |
| `unsupported_version` | Wire version other than `1` |
| `unsupported_capability` | Capability array differs from the supported exact set |
| `input_limit` | Request bytes, exclusions, repositories, strings, or counts exceed a bound |
| `member_limit` | A selected complete group cannot fit member or response bounds |
| `source_unavailable` | Required source facts could not be collected |
| `authentication_failed` | Adapter source-read authentication unavailable or refused |
| `deadline_exceeded` | Deadline already expired or reached during selection |
| `internal_error` | Other adapter failure, with no source or credential details |

Launcher-local failures use the same bounded code and message discipline. Invalid responses produce local `response_contract_violation`; process failures produce local `transport_failure`; timeout produces local `deadline_exceeded`. These local outcomes are not additional adapter response variants. If an oversized request reaches an adapter through another caller, that adapter also returns bounded `input_limit` without selecting work. An adapter need not echo an unparseable or overlong request ID.

Diagnostics contain fixed redacted explanations and bounded field paths or indexes only. They must not contain secret values, raw source bodies, environment dumps, authentication headers, token-bearing URLs, or raw child-process errors. stderr is diagnostic only, never JSON continuation or candidate data. The adapter redacts before writing either stream. taisan never forwards or persists raw stderr or invalid stdout; it records a fixed local error code and byte counts. Valid retained requests, responses, and evidence must also contain no secret values.

## Contract scenarios and evidence

These scenarios are requirements for dependent implementation, not claims that an external adapter already passes them.

| Named scenario | Required result |
| --- | --- |
| `grouped-success` | The grouped example validates and translates to one submit with two source snapshots and unchanged task fields |
| `empty-scan` | The empty example is successful and creates no Work |
| `source-failure` | The failure example is explicit failure, distinct from an empty scan |
| `bad-second-candidate` | One malformed candidate after a valid one rejects the entire response, with zero submits |
| `unsupported-wire` | An unsupported version or capability rejects the response before admission |
| `duplicate-identity` | Repeated proposal key or scoped member identity, including changed display metadata, rejects the whole response |
| `inconsistent-group` | Duplicate members, conflicting member revisions, excluded or foreign members, or noncanonical member order rejects the whole response |
| `invalid-task` | Missing intent, whitespace-only intent, wrong argv, missing required constraint, or expanded permissions rejects the whole response |
| `too-many-candidates` | More than request `max_candidates` rejects the response without truncation |
| `admission-overlap-race` | A valid candidate loses an ownership race and is rejected normally; a later independent candidate can still be accepted |
| `bounded-input` | Oversized exclusions or encoded input fails explicitly without dropping an exclusion or spawning an adapter |
| `bounded-output` | Oversized members, stdout, stderr, or expired deadline creates no Work and yields bounded diagnostics |
| `stable-replay` | Candidate reordering preserves keys; exact replay returns the original decision, including after terminal release |
| `no-authority` | Adapter receives source-read configuration only and cannot access store, Worker authority, or publication secrets |

Evidence for an implementation must retain the tested revision, adapter identity and version, argv and redacted configuration, protocol version, request policy and limits, named scenarios, actual results, and verification commands. Fixture evidence and owner-run live evidence must be recorded separately. A successful fixture is not evidence of a real source integration or live execution. The document-only verification record for this issue is [the #200 evidence record](evidence/selector-protocol-200.md).
