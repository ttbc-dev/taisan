# S1 isolated installation specification

Status: specification of the installation under which the S1 live
acceptance scenario runs. This document prepares configuration; it
grants no live-execution permission (#113 owns execution and live
evidence approval).

## Repository isolation

| Field | Value |
| --- | --- |
| Repository | `shimpeiws/coding-worker-smoke` |
| Role | Dedicated S1 test repository owned by the installation operator |
| Installation allowlist | Contains **only** this repository |
| Automation isolation | Removed from the worker automation lane's repository allowlist on 2026-11-09 — seeded issues are never picked up by any automation lane |

A repository rename does not change the binding: Core binds the stable
repository identity (host instance ID + stable repository ID), and the
allowlist is evaluated against that identity, not the display name.

## Credential

| Field | Value |
| --- | --- |
| Kind | Fine-grained personal access token, scoped to `shimpeiws/coding-worker-smoke` only |
| Permissions | contents RW, pull requests RW, issues RW, checks R |
| Storage | Installation config holds a **credential reference** (a name resolving to the secret store entry) — never a value |
| Reference name | `s1-publication-credential` (installation-local; the resolved value never appears in the repository, committed config, or evidence exports) |

The reference mechanism permits swapping to a scoped GitHub App later
without changing this spec or the recorded Work bindings. Credential
renewal under the same actor is permitted; changing actor, host,
adapter, capability requirements, or contract version is a new binding,
not a rotation.

## Reviewer identity

| Field | Value |
| --- | --- |
| Kind | Dedicated S1 reviewer GitHub App (owner-created) |
| Installation scope | Installed on `shimpeiws/coding-worker-smoke` only |
| Relationship to worker credential | A separate identity: review evidence is produced by an actor distinct from the publication actor, so author and reviewer can never collapse into one identity |

## Worker runtime identity

The target worker is the **reference Worker adapter's configured
identity**, recorded in this same install spec. The worker receives
`AllowedEffects{workspace_write: true, local_git_write: true,
repository_host_write: false}` — every repository-host mutation funnels
through a persisted `HostActionIntent` applied by the GitHub adapter.
Workers, including review workers, hold no repository-host mutation
credential.

## Binding profile

| Field | Value |
| --- | --- |
| Host instance ID | `github.com` (the production GitHub instance) |
| Repository | The stable repository ID of `shimpeiws/coding-worker-smoke`, not its display path |
| Adapter | `github` |
| Contract version | The adapter contract version negotiated at installation |
| Actor bindings | `publication` → the scoped-PAT identity; review observations bound to the reviewer App identity |

## Required capabilities

The S1 profile declares these capabilities against the bound actors
(capability names follow the [host contract](../repository-host-contract.md#common-semantics-and-optional-capabilities);
installation config carries the current `HostCapability` vocabulary):

| Capability | Why S1 needs it |
| --- | --- |
| `repository_read`, `branch_read`, `change_request_read` | Baseline reads: issue/branch/PR state, head observations |
| `push_ref` | Publish `work/<work_id>` under the expected-prior-ref guard |
| `change_request_create`, `change_request_update` | Open the PR and update metadata as separate receipts |
| `comment_create` (comment write) | Marked replies and ordinary comments the accepted profile requires |
| `issue_read` | Seeded-issue observation for admission |
| `checks_read` | Named check evidence bound to the examined head |
| `review_findings_read` | Review findings bound to the head they examined |

Optional capabilities are declared **only if the accepted profile
requires them** — an undeclared capability stays explicitly unsupported
and can never be silently substituted by a comment or a mergeable flag.
The contract-level operations `thread_reply`, `ready`, `approve`,
`request_changes`, `review_feedback`, and `inline_comment` are not yet
in the closed `HostCapability` vocabulary (which today extends to
`thread_resolve` and the names listed above), so an S1 profile cannot
require them until the vocabulary grows — a profile that did would fail
installation validation as unsupported.

## Runtime environment

One host, one installation: SQLite state store, managed workspace root,
real GitHub adapter, the reference Worker adapter, network access to
GitHub. No external board, card, or database — the built-in queue in
the store is the only state authority.

## Limits

| Setting | Value | Source |
| --- | --- | --- |
| Loop interval / tick deadline / source timeout | 30 s / 120 s / 30 s | [execution loop](../execution-loop.md#owner-decisions) |
| Host outbox / reconciliation per tick | ≤ 8 / ≤ 16 | same |
| `no_progress` attention | 1800 s | recovery policy |
| `max_invocations`, `max_review_rounds`, `runtime_limit_seconds` | per-run bounds recorded with the Work's budget | installation config |

## Rerun and cleanup

Each run submits a new Work against a freshly seeded issue (therefore a
fresh `work/<work_id>` branch). PRs, issues, checks, and reviews
produced by a run are retained as evidence and never deleted by
cleanup; only seeded inputs may be reset. Merge and deploy are human
decisions outside the scenario and are never performed.
