# GitHub adapter operation and capability mapping

Status: design mapping for the real GitHub repository-host adapter (#17)
against the neutral contract in [repository-host-contract](repository-host-contract.md)
and the executable vocabulary in `workloop-core` (`HostAction`,
`HostCapability`, `NormalizedEffectFact`). This is a mapping from official
API documentation, not evidence of working integration — a candidate
mapping is not a capability declaration, and adapter support remains
unverified until the shared suite and operation-specific live checks
establish it.

The shared conformance suite
(`crates/workloop-github/tests/host_conformance.rs`, rows in
[host-conformance-matrix](host-conformance-matrix.md)) proves modeled
behavior only; nothing here claims a live GitHub capability.

## Operation mapping

Each `HostAction` maps to a distinct GitHub effect with its own durable
action ID and receipt. Multiple API calls never merge into one atomic
effect; if a host requires multi-call execution for one operation, the
adapter must durably identify the constituent effects and preserve
partial receipts before it may advertise the operation.

| `HostAction` | GitHub surface | Required normalized receipt | Failure / unknown semantics |
| --- | --- | --- | --- |
| `push_ref { branch, produced }` | Git protocol `update refs/heads/<branch>` with an exact prior-ref/absent guard (protocol `old-oid` precondition or equivalent); non-fast-forward refused | `RefPublished { repository, branch, produced, observed, relation }` — observed ref read back after the write | Guard mismatch or rejected update → `failed`; lost response → `unknown`, reconciled by `action_id` via a fresh ref read (`ls-remote`-equivalent), never a blind retry |
| `open_change_request { source_branch, target_branch, title, body }` | `POST /repos/{owner}/{repo}/pulls` | `ChangeOpened { change, head, source_branch, target_branch }` — `change` binds the repository-scoped stable PR identity (node/database ID), not the display number; `head` is the exact source revision opened | Validation refusal → `failed`; timeout after the request may have landed → `unknown`, reconciled by listing PRs for the recorded head revision under the original binding |
| `update_change_request { change, expected_head, … }` | `PATCH /repos/{owner}/{repo}/pulls/{n}` after an immediate head re-read (`GET …/pulls/{n}`) | `MetadataUpdated { change, head, fields_digest }` | Head moved since `expected_head` → abort before writing, `failed` receipt, and a fresh `ChangeHead` observation; the write never lands at a stale target |
| `post_comment { change, expected_head, marker, body }` | `POST /repos/{owner}/{repo}/issues/{n}/comments` after an immediate head re-read | `CommentPosted { change, comment_id, actor, marker, body_digest, target }` — the host's stable comment ID and the authenticated actor | Same head guard as `update_change_request`; a comment is never adopted onto a different head |

A head read is not atomic with the subsequent write: every head-bound
operation re-reads immediately before writing and re-observes afterward.
The fixtures model exactly this guard point.

## Read paths

| Read | GitHub surface | Binding requirement |
| --- | --- | --- |
| Repository / branch head | `GET /repos/{owner}/{repo}` (stable `id`), `GET …/git/ref/heads/{branch}` or ls-remote | Identity keys are the repository's stable numeric/node ID; owner/name is display metadata only |
| Change request head/state | `GET /repos/{owner}/{repo}/pulls/{n}` | `head.sha` binds the exact revision; `merged`/`state` is a host fact, never a Core verdict |
| Checks | `GET …/commits/{sha}/check-runs`, commit statuses | Named check identities and conclusions bound to the exact revision examined; missing/unavailable required checks never become passing |
| Reviews / findings | `GET …/pulls/{n}/reviews`, `…/pulls/{n}/comments`, GraphQL review threads | Findings keep their review target and source availability; empty findings are meaningful only with an available source |
| Issue content | `GET /repos/{owner}/{repo}/issues/{n}` | Admission input: allowlisted issue identity, content revision token (`updated_at` plus content digest), and provenance |

## Capability mapping

`HostCapability` is a closed set; an installation declares support per
capability per actor, and support is separate from authorization
(`supported`/`unsupported`/`unknown` × `allowed`/`denied`/`unknown`).
A transient outage is `unknown`, never `unsupported`; a denied mutation
leaves intents `pending`, never a fabricated failure.

| Capability | GitHub surface | Notes for the GitHub adapter |
| --- | --- | --- |
| `repository_read` | `GET /repos/{o}/{r}` | Establishes the stable repository ID used by `RepositoryRef`; rename must not change it |
| `repository_read_metadata` | same endpoint, metadata fields | Display-path/default-branch reads; never an equality key |
| `branch_read` | `GET …/git/ref/heads/{b}`, `GET …/branches/{b}` | Current head object ID for guarded writes and reconciliation |
| `push_ref` | git protocol ref update | The only write path with a compare-and-set precondition; enforce absent/exact-prior-ref at the write boundary |
| `pull_read` | `GET …/pulls/{n}` + list | Change-request reads beyond head state |
| `change_request_create` | `POST …/pulls` | Separate durable effect from `push_ref`; a failed create never overwrites a successful push receipt |
| `change_request_update` | `PATCH …/pulls/{n}` | Head-bound write; guard as above |
| `change_request_read` | `GET …/pulls/{n}` | Head-bound reads feeding `ChangeHead` observations |
| `change_request_list` | `GET …/pulls?head=…` | Reconciliation of `unknown` open effects by recorded source revision |
| `review_read` | `GET …/pulls/{n}/reviews` | Review state for the examined head only |
| `review_findings_read` | reviews + review comments + threads | Preserves old-head evidence and source availability |
| `comment_create` | `POST …/issues/{n}/comments` | Head-bound write; contract baseline names this `comment_write` — same operation, one mapping |
| `thread_resolve` | GraphQL `resolveReviewThread` | Capability is declarable but has no `HostAction` operation in the current vocabulary — explicit-unsupported at the effect boundary (HC-CAP-03) |
| `issue_read` | `GET …/issues/{n}` | Admission source reads with revision/digest provenance |
| `issue_list` | `GET …/issues` / search | Selector-facing enumeration; scoped to allowlisted repositories |
| `checks_read` | check-runs / statuses endpoints | Exact-revision evidence for readiness lanes |
| `checks_write` | `POST …/check-runs`, commit statuses | Adapter-owned status emission; not required by the S1 profile |

## Contract optional capabilities not in the executable vocabulary

These exist in the contract's optional set and in GitHub's API, but have
no `HostAction` variant today. They remain explicit-unsupported: an
accepted Work requiring them cannot weaken them into a comment or a
review submission.

| Contract capability | GitHub surface | Status |
| --- | --- | --- |
| `inline_comment` | `POST …/pulls/{n}/comments` at a validated diff position | Not in vocabulary — position validation against an immutable diff snapshot belongs to the adapter that implements it |
| `review_feedback` | `POST …/pulls/{n}/reviews` event `COMMENT` | Not in vocabulary — head-bound review record, not an approval |
| `approve` | `POST …/pulls/{n}/reviews` event `APPROVE` | Not in vocabulary — requires independently observable evidence and actor-eligibility proof |
| `request_changes` | `POST …/pulls/{n}/reviews` event `REQUEST_CHANGES` | Not in vocabulary — host-native decision, not ordinary criticism |
| `thread_reply` | `POST …/pulls/{n}/comments/{id}/replies`, GraphQL thread reply | Not in vocabulary — marked reply preserving thread identity |
| `thread_resolve` | GraphQL `resolveReviewThread` | Capability name exists; operation absent — see table above |
| `ready` | GraphQL `markPullRequestReadyForReview` | Not in vocabulary — already-ready state may satisfy the postcondition only through a fresh observation |
| `comment_read` | `GET …/issues/{n}/comments` | Baseline in the contract; the typed read-model rows are an uncovered matrix gap (HC-READ-01), tracked separately |

## Identity scoping

- `RepositoryRef` = configured host instance + GitHub's stable repository
  `id` (numeric/node ID). Owner/name paths and renames are display
  metadata; Enterprise instances never collide on shared numbers.
- `ChangeRequestId` = the PR's stable node/database ID scoped to the
  repository — never the display number.
- `HostActorRef` = host instance + stable user/app-bot ID; a login rename
  is display metadata.
- Observation delivery dedup = `(host instance, adapter source, delivery
  GUID)`; the same delivery ID under a different configured source is a
  distinct identity, not a duplicate.
- The same SHA or review ID on another change request is not
  interchangeable evidence: every receipt and observation keeps its
  recorded repository/change binding (HC-SCOPE-01).

## What this mapping does not claim

- No live GitHub call was made for this document; it is the adapter
  author's contract checklist, not integration evidence.
- Fixture passes support modeled contract behavior only.
- Live capability claims require the separately authorized live checks
  the contract specifies; approval/readiness claims additionally require
  actor-eligibility evidence.
