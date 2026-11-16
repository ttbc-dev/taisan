# Repository host adapter contract

## ADR-002: select a conforming host adapter without changing Core

Decision: a repository host adapter satisfying the versioned common contract and the capabilities required by a Work can be selected through installation configuration without changing Core. GitHub is the first production target. This document specifies a target contract, not an implemented GitLab adapter or a demonstrated portability claim.

This extends [ADR-001](adapter-boundaries.md#adr-001-core-authorizes-publication-github-integration-performs-it). Core remains the sole authority for admission, queueing, budgets, retry/resume/cancel, effect authorization, evidence assessment, and Outcome. The selected host adapter performs external writes and reports facts. Workers, including Review Workers, have no repository-host mutation credential.

### Alternatives

| Shape | Decision and reason |
| --- | --- |
| GitHub-only Core, with a new Core path for every host | Rejected: a new adapter would require different lifecycle code and duplicate recovery policy. |
| One API restricted to the smallest shared feature set | Rejected: it would discard GitHub features or silently weaken required approval, review, and readiness behavior. |
| Separate public Git, change-request, and review ports | Not needed now: cross-port identity, effect ordering, and reconciliation would add coordination. An adapter may split those responsibilities internally. |
| One static `RepositoryHostPort`, concrete operations, and semantic capability requirements | Selected: reuse the existing integration boundary, keep provider-specific protocols inside the adapter, and refuse unsupported requirements explicitly. No dynamic loader, generic provider JSON, or compatibility wrapper is introduced. |

## Identity and configuration binding

A repository identity includes a configured host-instance ID and that instance's stable repository ID. Issue and change-request IDs are opaque and scoped to that repository. Git commit revisions also carry the repository identity. Display URLs, owner/name paths, PR/MR numbers, and provider names are not Core equality keys. A repository rename cannot change its resource reservation key. GitHub Enterprise and distinct GitLab instances must not collide even when their external numbers match.

One Work binds one publication repository, host instance, configured adapter, contract version, required capabilities, and selected actor roles at acceptance. Related issue references remain allowlisted sources, not permission to publish to another repository. Admission resolves configured minimum requirements together with the Work's requested effects; callers cannot weaken installation policy. The binding and immutable requirements are stored with Work and exposed by Queries. Retry/resume retain them.

Configuration selects the adapter for new Work. Existing Work and unresolved effects keep their original host/repository/actor binding. Retain the old adapter/configuration needed for reconciliation; removing it surfaces unavailable evidence and blocks release. A same-actor credential renewal is allowed. Changing host, adapter, actor, capability requirements, or contract version is not credential rotation and cannot retarget a running Work. Cross-host migration, live adapter handoff, and multi-host publication within one Work are out of scope.

Observation delivery IDs are deduplicated within host instance and adapter-source identity, not as globally unique provider strings. An event ID must be unique per distinct subject observation: dedupe does not include the observation's subject, so a read of state several Works share (for example a target-branch head) must carry a per-subject discriminator in the ID — otherwise the second subject's delivery is dropped as a duplicate and it never records the fact. Resource keys include the stable host/repository binding. Evidence binds repository, change-request identity, exact revision, producing source/actor, and action or Invocation identity. The same SHA or review ID on another change request is not interchangeable evidence.

## Common semantics and optional capabilities

Every conforming host adapter supplies stable identities, current branch/change-request reads, guarded fast-forward publication, change-request creation, ordinary change-request comments, independent per-effect receipts, and read-back reconciliation. These form the mandatory baseline. Its capability entries are individual operations: `repository_read`, `branch_read`, `push_ref`, `change_request_read`, `change_request_create`, `change_request_update`, `comment_read`, and `comment_write`. There is no single baseline authorization bit; Git transport mechanics and HTTP APIs remain adapter details.

A capability report is evaluated for each operation and the actor that will perform it. Publication actors need their approved publication operations; discussion/review actors need their own operations. Reconciliation checks read permissions independently and remains possible when mutation permission is lost.

The following capabilities extend that baseline. Each has one defined effect, rather than a broad `supports_reviews` flag:

| Capability | Required semantics when selected by policy |
| --- | --- |
| `issue_read` | Return the allowlisted issue identity, content revision, and provenance for admission/selection. |
| `checks_read` | Return named check identities, conclusions, source availability, and the exact change-request revision examined. Missing/unavailable required checks never become passing checks. |
| `review_findings_read` | Return findings and their review target/source, preserving old-head evidence and source availability. Empty findings are meaningful only with an available source. |
| `inline_comment` | Post one comment at a validated location in the specified immutable diff snapshot, including source head, comparison base, and target revision. Ambiguous/outdated positions are rejected, never moved to a different line silently. |
| `review_feedback` | Publish one head-bound review feedback record. It is not an approval or a host merge-policy decision. |
| `approve` | Record an eligible actor's approval for the specified change request and exact revision, with independently observable evidence. Core separately evaluates reviewer independence and required approval policy. |
| `request_changes` | Record the requested actor's host-native change-requesting decision with target revision and evidence. Posting ordinary criticism is not an implementation of this capability. |
| `thread_reply` | Post one marked reply to the specified change-request thread, preserving the exact thread identity and action receipt. |
| `thread_resolve` | Resolve the identified thread and observe that effect. A reply or claim of addressing a finding is not thread resolution. |
| `ready` | Establish and observe the requested change request's non-draft state after prerequisites. Already-ready state can satisfy this postcondition through a fresh observation without another write. |

Optional means that an adapter may omit the capability. Once an accepted Work requires it, it cannot be skipped, converted into a comment, or removed to make execution proceed. A profile requiring independent approval is not satisfied by the author's review or a generic provider `mergeable` flag. Repository-host policy facts and Core readiness decisions remain separate.

Capabilities are evaluated for the configured instance/version, repository, actor, and requested operation. Distinguish implementation support (`supported`, `unsupported`, `unknown`) from current authorization (`allowed`, `denied`, `unknown`). A capability is usable only when both support and authorization are established with evidence. A transient outage must not be advertised as permanent lack of support.

Validate installation requirements at configuration time and the complete Work requirements at acceptance and again before dispatch. Unsupported required capabilities reject admission before Worker start or remote writes. Unknown capability evidence leaves admission unaccepted; at dispatch, a temporarily unavailable source defers the queued Execution with a diagnostic. Insufficient permission is an explicit authorization failure. Recheck prerequisites and actor eligibility before each effect; startup validation is not a permanent grant.

If permissions or capability availability change during a Work, prevent new dependent Invocations/effects and surface the recorded repair or Attention path. Pause only when writer/effect safety permits. Already-applied receipts remain facts. Read-only reconciliation of prior unknown effects continues against their original binding; unavailable reads keep them unknown and retain ownership. Never turn an unknown effect into failure, release, or a new action merely because its capability disappeared.

## Typed boundary and adapter obligations

The [module contracts](module-contracts.md#type-sketch) define the neutral target IDs, actions, and Observations. The intended port is:

```ts
interface RepositoryHostPort {
    capabilities(context: HostContext): CapabilityReport;
    observe(target: HostReadTarget): Observation[];
    apply(intent: HostActionIntent): HostActionReceipt;
    reconcile(intent: HostActionIntent): HostActionReceipt;
}
```

`HostContext` fixes instance, repository, actor, and contract version. `HostReadTarget` is a typed issue, branch, or change-request read with an optional requested immutable revision; the capability report carries per-operation support/authorization and evidence. A result receipt carries action/payload identity, original binding, observed actor/target, status `applied | failed | unknown`, typed operation-specific effect facts, and retained evidence. Those facts identify the published ref/revision, created change request, comment/review identity, reply target, approval decision, resolved thread, or ready state as appropriate. `applied` requires the matching normalized facts; an evidence reference to raw provider output alone is insufficient. `apply` consumes an already durable approved intent. `reconcile` only establishes facts about that intent; it does not authorize another write.

Operations remain concrete: `push_ref`, `open_change_request`, `update_change_request`, `post_comment`, `post_inline_comment`, `publish_review_feedback`, `approve_change`, `request_changes`, `resolve_thread`, and `mark_ready`. Each has its own durable action identity and postcondition. Multiple API calls do not become one atomic effect by naming them a stage. If a host requires multi-call execution for one selected capability, the adapter must durably identify constituent effects and preserve partial receipts across restart; it cannot advertise that capability until its reconciliation contract is proven. It never owns a separate Work retry or settlement loop.

The adapter translates authentication, pagination, webhook formats, diff positions, rate limits, and native IDs. Provider-specific receipt details can be opaque evidence for diagnosis, but Core must receive the normalized facts needed to verify every promised postcondition. Core cannot inspect raw provider JSON or branch on `github`/`gitlab`. This is not a host-specific plugin API hidden behind `execute(kind, payload)`.

The existing stop-proof, current-head, partial-publication, and unknown-result guards remain mandatory on every host. A branch read before push is not sufficient concurrency control: the update must enforce the absent/exact prior-ref and fast-forward condition at the write boundary. A head read before a comment is not atomic with posting; retain its target and reobserve afterward. If the provider cannot establish the contract, report unsupported/unknown rather than invent success. Polling can provide observations; no specific webhook or App authentication mechanism is mandatory.

## GitHub and GitLab mapping pressure test

This is a design mapping from official API documentation, not evidence of working integration. A candidate mapping is not a capability declaration. Adapter support remains unverified until the shared suite and operation-specific live checks establish it.

| Common concept | GitHub reference | GitLab candidate | Semantic condition to prove |
| --- | --- | --- | --- |
| Repository and change request | Stable repository ID and PR identity | Instance/project identity and MR identity | Rename and overlapping display numbers cannot alias Core identities. |
| Publish and open | Git ref update, then PR creation | Git ref update, then MR creation | Preserve separate push/create effects, exact prior ref, and partial results. |
| Head and ordinary comment | PR head and PR comment | MR source revision and note/discussion | Bind to the intended change; never adopt another head or duplicate ambiguous writes. |
| Inline feedback and threads | Review comments and review threads | Diff discussions with base/start/head revisions | Resolve positions for the exact diff; keep unsupported layouts explicit. |
| Approval and review decision | Review submission and actor eligibility | Approval endpoint, review state, and configured project rules | Native effects are not identical APIs; prove exact-head evidence and independence without comment substitution. |
| Checks and ready state | Required check evidence and draft PR state | Pipeline/check evidence and draft MR state | Observe exact revision and separate available facts from Core completion policy. |

The differences above are derived from [GitHub review APIs](https://docs.github.com/en/rest/pulls/reviews), [GitLab merge requests](https://docs.gitlab.com/api/merge_requests/), [GitLab approvals](https://docs.gitlab.com/api/merge_request_approvals/), and [GitLab discussions](https://docs.gitlab.com/api/discussions/). For example, GitLab diff discussions carry base/start/head positions, while approval has its own endpoint and eligibility checks. The adapter must verify a mapping rather than copy GitHub's review event enum into Core. Feedback publication, approval, and requesting changes are separate operations/capabilities, not a shared provider event enum. A Core policy can use a Worker's findings even when native review publication is unsupported, but cannot omit any remote review/approval effect required by the accepted profile.

## Verification, release scope, and migration

The current Rust `GitHub*`, `PrHeadSha`, `PrActionIntent`, `PrAction`, `AllowedEffects.github_write`, and source-event/storage fields are not yet this common contract. The neutral sketch changes the target design only. The owner-approved [legacy cutover decision](identity-storage.md#maintenance-cutover-and-legacy-reference) in #50 narrows executable data compatibility: retain the unversioned slice-0 store as a read-only archive and start new-format Work in a fresh store; do not import its active Work, pending effects or legacy replay keys. Preserve the original historical bytes/identities in the archive. New-format command replay remains authoritative within its StoreId. This exception replaces executable legacy conversion, not the neutral contract or future new-format history guarantees. The implementation plan coordinates replacement of Core callers, persistence keys/evidence, Queries, CLI/skills input, fixture tests, and GitHub adapter. Preserve historical records/identities under the archive exception above and preserve replay identities within every active new-format store. Do not keep permanent GitHub and generic Core paths or pretend aliases make the current implementation portable. GitHub App/user authentication remains a GitHub adapter implementation choice; other hosts use their own credential schemes behind the same actor binding.

v0.1's production adapter and live S1 remain GitHub. Full production GitLab support and broad multi-host feature parity are deferred. The contract and its validation plan are required now. Passing a fixture proves modeled behavior, not host support. Before claiming adapter-only exchangeability across GitHub and GitLab, run the unchanged common suite against independently implemented host fixtures and complete a narrow live GitLab contract spike without modifying Core. #36 plans that spike and records unsupported capability gaps separately from S1; it does not require a production GitLab integration to finish GitHub S1.

The shared suite must cover baseline effects, each declared optional capability, missing/unknown/denied requirements before start, permission drift, scoped identity/event collisions, rename, stale-head observations, partial push/create, lost responses, duplicate delivery, actor changes, restart, cancellation, and reconciliation after configuration changes. Swapping adapters for a new equivalent Work/profile preserves Command, queue, budget, Attention, and Outcome semantics without changing Core or pretending the external identities match.

The shared suite also moves the target branch without changing the source head and verifies rejection of the stale inline diff snapshot. It verifies reading issue content, comments, and thread state through the typed port and rejects applied receipts missing the required effect facts.

The live GitLab spike exercises a supported narrow profile through guarded push, MR creation, ordinary comment, head read, and a head-bound review/check reassessment. If approval or readiness is claimed, verify those capabilities and actor eligibility too. Inject denied permission and lost responses, retain separate effect receipts, restart, and reconcile an unresolved action under its original binding after the default configuration changes. Record exact adapter/Core revisions, instance/config identity, required profile, remote evidence, and unsupported items. A mapping table or scripted adapter alone cannot satisfy this proof.

## U3.6 evidence record

Every U3.6 suite run retains one record per exercised scenario per adapter — the same shared-suite scenario table produces one record for the fixture adapter run and one for the GitHub adapter run, so a semantic claim is proven on independent implementations rather than asserted once. The record is JSON with these fields; values are evidence, never instructions. A record missing any required field is invalid, and a field-presence check asserts every retained record carries them.

| Field | Required content |
| --- | --- |
| `schema_version` | `1` |
| `tested_revision` | Exact commit SHA on the PR branch that contains the exercised slice tree. A base revision or patch digest alone never satisfies this field; a patch digest may be retained alongside it. Where the CI checkout is a merge ref (the default `pull_request` checkout), the exercised tree is the merge of this commit onto the recorded `base_revision` — the pair describes it, and the slice tree is the one this commit carries |
| `base_revision` | The base revision the run was exercised against — the merge-base fork point where the checkout retains reachable history (a truncated checkout cites the deepest commit it retains); in CI, the platform-recorded event base (`pull_request.base.sha`, or the push `before`), which is the base that run actually tested. A checkout that can prove neither retains no records |
| `core_revision` | The exact revision of the Core implementation exercised — a commit SHA for in-repo builds |
| `adapter_revision` | The exact revision of the adapter implementation exercised — a commit SHA for in-repo fixtures, an adapter-declared build identifier otherwise |
| `config_identity` | The configuration the run bound: `install_identity` (`host_instance`, `repository`, `adapter`, `contract_version`) plus `config_digest`, a sha256 over the canonical configuration projection — the record's config-file identity |
| `command_or_scenario` | The named scenario or command the record answers for |
| `action_identities` | The durable action identities the scenario exercised — an empty array for a scenario performing no authorized writes |
| `observation_identities` | The delivery identities the scenario's observations recorded — real identities only; an observation delivered with no delivery identity has nothing to cite here and is narrated in `read_back_receipts`, never fabricated as an identity |
| `read_back_receipts` | The read-back receipts the scenario's facts were re-observed through: each entry names the subject or delivery it answered for and what the read-back established |
| `result` | `pass`, `fail`, `skipped`, or `inconclusive` |
| `claim_class` | Closed set: `fixture_contract` (local fixture/contract evidence), `live_evidenced` (a separately authorized live scenario), or `not_authorized` (the scenario's owner authorization was withheld — it documents the non-run and claims nothing). Fixture results never satisfy a live claim |
| `authorization_reference` | Required iff `claim_class` is `live_evidenced`: the separately authorized approval the live run cites. Absent — never null — on a `fixture_contract` or `not_authorized` record; its presence there invalidates the record, and its absence invalidates a `live_evidenced` record |
| `isolation_scope` | Required iff `claim_class` is `live_evidenced`: the isolated scope the run executed under — `installation_id` and `repository` (`owner/name`), both non-empty. Absent — never null — on a `fixture_contract` record; a `not_authorized` record may carry the intended scope so the withheld authorization names what it would have covered |

The field-presence check is `workloop_github::evidence::validate_record_json`; `EvidenceRecord` serializes the same closed shape, so typed records and externally produced JSON answer the same contract. Runs stamp revisions fail-closed (`record_revisions`): a checkout that cannot prove a commit describing the exercised tree — a dirty tracked tree, a non-git source, missing base history — executes the suite but retains no records rather than stamping a guess.

The consolidated fixture-class record indexing the U3.6a–c scenario records against the #107 failure modes is [u3.6d](evidence/u3.6d.md); the separately authorized isolated live cases are U3.6e (#119), whose live-class records append to it.
