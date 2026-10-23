# Architecture and interface boundaries

## Responsibilities

| Component | Owns | Does not own |
| --- | --- | --- |
| Core | Work and attempt IDs, state transitions, leases, capacity, retry/resume policy, evidence, current attention | GitHub's issue and PR data, external board state |
| GitHub integration | Issue/PR reads, head revisions, checks, review events and publication | Work lease or execution authority |
| Executor | Isolated workspace, agent invocation, process and artifact reporting | Choosing work or declaring a PR reviewed on a newer head |
| Control adapter | Command translation, status projection, external identity mapping | State transitions or independent retry policy |
| CLI | Reference implementation of the control interface | A separate state model |

The initial deployment may place these in one process and one local database. The boundaries are behavioral; v0.1 does not require a distributed service or dynamic plugin loader.

## Command contract

The core exposes commands such as `submit`, `retry`, `resume`, `cancel`, and `acknowledge`, plus read-only queries for status, evidence, and current attention. A command contains:

- a stable Work ID and an idempotency key;
- actor and origin (CLI, GitHub, or another adapter);
- expected state/version, where relevant;
- optional reason and target attempt.

The core returns an accepted/rejected decision, canonical state version, and command ID. `retry` means: create at most one new attempt for eligible, settled Work under the same intent, after checking current evidence and policy. `resume` continues an eligible interrupted attempt from a durable checkpoint. Adapters cannot implement either by moving a card or spawning a process directly. An unavailable result can be queried by command ID before resubmission.

## Work and execution

Work has its own identity independent of an issue number or board card. Store GitHub issue references as source links, not primary keys. Each Execution has an attempt ID, lease owner and deadline, heartbeat, process/workspace evidence, checkpoints, and a terminal result. A Work item may have multiple attempts but no more than one active, authoritative attempt. Capacity is reserved atomically with the claim.

The state store is the authority for commands and transitions. An append-only event or decision history supports audit and reconstruction; snapshots may accelerate queries. External updates use an outbox and reconciliation so a database commit does not pretend a GitHub or board write succeeded atomically. Webhooks are deduplicated on delivery identity and checked against current Work state.

## Adapters

An adapter implements a narrow protocol:

1. Resolve an external action to a canonical Work ID and command.
2. Submit that command with an idempotency key and state precondition.
3. Render the returned decision and current state.
4. Reconcile its projection after delayed, duplicated, or lost events.

Candidate adapters are a GitHub issue-comment interface, a GitHub Projects projection, and an optional board integration. The CLI is the baseline and must work with every optional adapter disconnected. GitHub Projects offers project item/field APIs, but its fields are presentation and control input, not leases or execution truth.

## Recovery rules

- A heartbeat failure is `unknown` until the lease and process evidence are reconciled; it is not immediately `dead`.
- An expired lease does not allow a second attempt while an old process could still write; recovery must fence the old owner or establish its termination.
- An external card or comment can lag behind the core. Reconciliation updates the projection; it never changes core state just to match a stale view.
- A new PR head invalidates review conclusions tied to the previous head.
- An adapter outage records a pending projection/notification and leaves execution progressing when policy allows.

## Installation boundary

Each deployment has a separate state database, workspace root, credentials, repository allowlist, and adapter configuration. Logs and exported evidence must redact tokens and avoid copying private source content into other engagements. v0.1 uses one self-hosted instance; shared hosted tenancy is outside scope.
