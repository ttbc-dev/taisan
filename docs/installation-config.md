# Installation configuration, credential references, and permission boundaries

Status: concrete design for #53. This document specifies future Rust/configuration behavior; it changes no runtime, installs no credentials, and performs no live checks. It consumes #50's identity/binding model, #51's evidence and repair prerequisites, #52's worker isolation profile, and #54's retention keys. The owner approved the credential-reference provider set, the TOML schema, the role profile defaults, the preflight probe boundary, the XDG layout, the rotation boundary, and the setup command contract recorded below.

## Owner decisions

| Choice | Result |
| --- | --- |
| Credential references | `credential_ref` is an opaque Core value with three resolution providers: `env:NAME`, `file:/absolute/path` (mode `0600` and owner-checked), and `command:argv` (e.g. a secret-manager CLI). Values resolve only at the adapter boundary and never enter Core, evidence, argv, logs, intents, or exports |
| Configuration format | One TOML file, `config.toml`, with a required `schema_version`. Unknown keys, unknown/newer versions, and structurally invalid files are refused without mutation — the same refusal model as the store's `user_version` |
| Role profile | Three roles — `publication`, `review`, `discussion` — each binding `(actor, credential_ref)`. v0.1 permits one user token bound to all three roles; a distinct reviewer identity is the documented recommended configuration |
| Preflight boundary | `taisan check` validates offline by default; `--probe` performs read-only adapter capability calls. Dispatch-time revalidation is always mandatory — a setup probe is never a standing grant |
| Installation layout | XDG split: `~/.config/taisan/config.toml` for configuration, `~/.local/share/taisan/` for state (store, CAS bodies, workspaces, out directories, handoff repository) |
| Rotation boundary | Same-actor credential renewal (updated `credential_ref` resolving to the same verified actor) is permitted and never retargets existing Work or unresolved effects. Actor replacement is a binding change: existing Work keeps its original binding; the new actor applies to new Work only. Unresolved effects always reconcile under the original actor/binding |
| Source machinery | Source-repo automation mechanisms — webhook/cron/launchd routes, tunnels, installer convergence, remote operator control — are omitted. Their retained obligations are re-modelled onto configuration validation, preflight, fail-closed defaults, and secret hygiene |
| Setup contract | `init` scaffolds a validated profile only from observed evidence and never fabricates identity; `check` runs an ordered read-only fail-closed preflight |

## Installation layout

| Path | Contents | Notes |
| --- | --- | --- |
| `~/.config/taisan/config.toml` | The single installation configuration | Published atomically; `TAISAN_CONFIG` overrides the path |
| `~/.local/share/taisan/store.db` | SQLite store | `TAISAN_STATE_DIR` overrides the root |
| `~/.local/share/taisan/bodies/` | CAS objects ([retention policy](retention-policy.md)) | |
| `~/.local/share/taisan/workspaces/` | Managed workspaces | Managed root; no unmanaged writer may enter |
| `~/.local/share/taisan/handoff.git` | Installation-owned bare handoff repository | Durable artifact objects ([worker protocol](worker-protocol.md#artifact-handoff)) |
| `~/.local/share/taisan/archive/` | Read-only legacy archives | `unlimited` retention default |

Every path resolves through one fixed precedence: explicit flag, environment override, then the XDG default — never the ambient working directory. A managed workspace nested inside the state root is the default layout; an explicit flag or environment override may also place the workspace root outside it (for example on a separate volume). What setup refuses is a configuration or state root nested inside the managed workspace root — workspace retention and cleanup must never be able to remove configuration or state, while nothing else needs to be kept out of the workspace tree.

## Configuration schema

Target schema sketch, sanitized — a literal, loadable example without real identities or secrets:

```toml
schema_version = 1

[installation]
capacity_slots = 1

[hosts.h1]
adapter = "github"                    # adapter id from the adapter registry
endpoint = "https://api.github.com"   # endpoint is transport detail; source-bound identities derive its canonical host instance (`github.com`)
contract_version = 1

[[hosts.h1.repositories]]
id = "0123456789"                     # adapter-resolved stable repository id
display = "team/tool"                 # display metadata only, never an equality key
use = "publication"                   # publication target

[[hosts.h1.repositories]]
id = "5566778899"
display = "org/reference-docs"
use = "source"                        # allowlisted read-only source

[roles.publication]
host = "h1"
actor = "taisan-publisher"            # verified adapter-side actor identity
credential_ref = "env:TAISAN_GH_TOKEN"

[roles.review]                        # recommended: a distinct reviewer identity
host = "h1"
actor = "taisan-reviewer"
credential_ref = "file:/home/user/.secrets/taisan-review.token"

[roles.discussion]
host = "h1"
actor = "taisan-publisher"
credential_ref = "env:TAISAN_GH_TOKEN"

[worker]
argv = ["taisan-worker-shim", "--runtime", "claude-code-cli"]
isolation_profile = "taisan-reference-v0.1"  # approved isolation profile; unknown names refuse at load
runtime_version = ">=2.0.0"           # pinned to declared range, verified at preflight
model = "claude-sonnet-4-6"           # pinned at load; re-checked after dispatch
max_turns = 200

[verification.egress]
allowlist = ["crates.io", "static.crates.io"]

[capabilities.required]
# Work requirements compose with this floor at admission; callers cannot weaken it
publication = ["repository_read", "branch_read", "push_ref",
               "change_request_read", "change_request_create",
               "change_request_update", "comment_write", "ready"]
review      = ["repository_read", "branch_read", "review_feedback",
               "inline_comment", "thread_reply", "thread_resolve"]
discussion  = ["comment_read", "comment_write"]

[recovery]  # keys defined by recovery-policy.md; defaults apply when omitted
[retention]  # keys defined by retention-policy.md

[selector]                             # optional: its presence selects alternate admission
adapter = "issue-scan"                 # selector adapter identity stamped on proposals
policy_revision = 3                    # revision of the policy document, part of proposal identity
allowlist = ["0123456789"]             # repository ids the selector may propose from
excluded = ["0123456789:137"]          # repo:number issues never selected (matched by opaque IssueId or display number)
max_proposals = 10                     # bound on proposals per selector run
task_constraints = []                  # carried into every proposal's task input
verification_argv = []                 # carried into every proposal's task input
required_permissions = []              # declared on proposals, never expanded

[intake]                               # optional: v0.2 scheduled and one-shot intake
enabled = false                        # automatic intake; disabled by default
interval_minutes = 30                  # positive whole minutes between due runs
timeout_seconds = 120                  # adapter invocation deadline, at most 300
pending_queue_limit = 10               # installation-wide pending Work admission bound

[intake.selector]                      # required when enabled = true or `taisan intake` is invoked
adapter = "public-issue-selector"      # selector adapter identity bound into every request
selector_version = 1                   # selection-semantics version, part of request identity
policy_revision = 1                    # policy document revision, part of proposal identity
argv = ["public-issue-selector", "select", "--github-endpoint", "https://api.github.com"]  # direct exec vector; never through a shell
```

Validation rules:

- `schema_version` is required and must equal a version this runtime understands; unknown/newer/corrupt content is refused without mutation, never auto-migrated.
- Unknown keys at any level fail loading with the offending key named — except inside the optional `[selector]` section, whose shape (including unknown keys) is validated when a selector run resolves the policy, exactly as the `[selector]` rule below specifies, so a malformed section can never block manual submission. `0` is never silently "unlimited" — each limit key's zero/disable semantics follow its defining document ([recovery policy](recovery-policy.md#configurable-policy-and-defaults), [retention policy](retention-policy.md#configuration)).
- Every `credential_ref` must parse as a known provider (`env:`, `file:`, `command:`); file providers require an absolute path. Validation checks the reference's shape, never its value.
- Role actors resolve against the declared host; an actor on a different host than its role declares is a load error. Two roles may share an actor only when policy permits; the recommended configuration keeps `review` on a distinct identity.
- `use = "source"` repositories grant read authority for admission sources only — never publication. `use = "publication"` implies source reads too.
- The packaged `public-issue-selector` `gh` backend serves only repositories whose host instance was declared with `--github-endpoint` (the matching `[hosts.*].endpoint` value); a request scoped to any other host fails closed instead of being silently served from github.com.
- Required-capability names are validated against the closed `HostCapability` set; an unknown capability name fails loading, not a silent no-op. Capability names come from the contract's capability set (`ready`, `thread_resolve`, ...), which deliberately differs from operation names (`mark_ready`, `resolve_thread`) — a capability selects an effect class, an operation names a concrete host action.
- `recovery.transport.backoff_seconds` is the per-attempt wait schedule between transient-failure retries. `recovery.transport.backoff_jitter_ratio` (default `0.0`, range `0.0`–`1.0`) applies symmetric jitter: each scheduled wait is scaled by a per-attempt random factor in `1 ± ratio`. A `recovery.transport.max_retries` beyond the schedule length repeats the final element; a shorter retry count truncates the schedule. A `0` schedule entry is a valid immediate retry; `0` is still never silently "unlimited" for limit keys.
- The `[selector]` section selects the alternate admission path. Its absence means manual submission only — the selector holds no default authority — and its unavailability must not block manual submission or existing Work. Every identity string inside it (adapter id, repository ids, issue references) is validated when the policy is resolved for a selector run, and unknown keys are refused rather than defaulted past, so a misspelled key surfaces as the bounded error instead of a silently degraded policy. An unparseable or unknown-keyed section is a bounded config error for that path only — validation deliberately does not gate config loading, so a malformed section cannot take manual submission down with it. An `excluded` `repo:number` entry matches a fact's opaque IssueId or its repository-scoped display number.
- The `[intake]` section (v0.2, #211) configures scheduled and one-shot selector intake through the argv adapter protocol of [selector protocol](selector-protocol.md). Its absence means automatic intake disabled with the documented defaults — interval 30 minutes, timeout 120 seconds, pending-queue limit 10 Work — and a pre-v0.2 profile loads unchanged. `enabled = true` requires a complete `[intake.selector]` binding; the reverse is not required, because `pending_queue_limit` is an installation-wide admission bound that applies to manual `submit` whether or not any selector is configured. Exactly one selector binding exists per installation: duplicate keys, unknown fields, and multiple selector declarations are refused structurally. `argv` follows the protocol bounds — 1–64 nonempty strings, at most 4,096 bytes each and 16,384 bytes total, no NUL or control characters — and launches directly without shell expansion. `interval_minutes` is a positive whole number of minutes; `timeout_seconds` is a positive integer at most 300 so the request deadline stays inside the protocol's 300,000 ms bound; `pending_queue_limit` is a positive integer. Zero, negative, fractional, and unit-suffixed values are load errors — never silently "unlimited". The section carries no credential references: the selector inherits source-read authority from `use = "source"`/`publication` repositories and resolves credentials only at the adapter boundary. Changed intake content yields a new settings revision — a digest of the complete validated section — while identical content keeps the revision; reload and due-time semantics are owned by the [execution loop](execution-loop.md#intake-scheduling). Intake settings govern new admission only: an accepted Work keeps its recorded binding, budgets, and unresolved effects across every reload.
- Configuration edits apply to new Work. Accepted Work retains its saved binding and effective policy (unchanged from #50/#51 semantics); a config reload cannot retarget live Work or unresolved effects. Switching the `[selector]` section in or out changes only which admission path new submissions take — it never rebinds existing Work.

## v0.3 operational profile (target schema revision 2)

Status: design selection in [v0.3 §14](v0.3.md#14-operational-launch-verification-and-host-composition). The keys below are additive and are not accepted by the current `schema_version = 1` loader. A dependent implementation may choose a different numeric revision, but it must not reinterpret a v1 profile as an operational v2 profile or silently ignore one of these keys. The v1 example above remains the loadable v1 profile.

```toml
schema_version = 2

[host_driver]
mode = "in_process"                    # only supported v0.3 value; `separate` is refused
source_poll_seconds = 30               # source-read cadence, bounded by the pass
outbox_max_per_tick = 8                # host effects attempted per pass
reconcile_max_per_tick = 16            # unknown/partial effects read back per pass

[worker]
argv = ["taisan-worker-shim", "--runtime", "example-cli"]
isolation_profile = "taisan-reference-v0.1"
runtime_version = ">=2.0.0"
model = "example-model"
max_turns = 200
max_runtime_seconds = 1800
stdout_cap_bytes = 33554432            # per-stream cap; not a separate budget
stderr_cap_bytes = 33554432            # both streams share one runtime.log total
report_cap_bytes = 1048576
credential_ref = "env:TAISAN_MODEL_TOKEN"   # runtime credential only; never a host token
provider_config = "/home/operator/.config/taisan/provider.toml"  # non-secret, absolute

[worker.egress]
model_api_hosts = ["model-api.example"]  # bare host names, CONNECT 443 only

[verification]
toolchain = "/opt/taisan/toolchain"       # absolute, read-only bind
cache_paths = ["/var/cache/taisan/deps"]  # absolute, read-only dependency caches

[verification.egress]
allowlist = ["crates.io", "static.crates.io"]
```

Validation and delivery rules:

- `schema_version = 2` requires the complete operational profile. A v2 profile that omits `[worker].credential_ref`, a non-empty model-API host set, the toolchain, or `[host_driver].mode` is a load error; there is no fallback to hygiene-only execution, an ambient runtime credential, or an unrestricted network.
- `[host_driver].mode` accepts exactly `in_process` in v0.3. `separate` is reserved for a later owner decision and is refused now, as is an unknown value; the scenario bridge is not a valid production mode.
- `[worker].credential_ref` uses the same `env:`/`file:`/`command:` provider rules as a role reference and resolves only at the supervisor boundary. It grants model access only. It is not a repository-host role and cannot be used for a `push_ref`, comment, review, or other host effect.
- `[worker].provider_config` is an absolute path to a non-secret file or directory (model identity, endpoint host, limits, empty MCP configuration). It must be owner-readable and must not contain a credential value; a secret found there is a configuration error, not a tolerated convenience. The supervisor renders or validates it and mounts it read-only; `/workspace` cannot override it.
- `[worker].stdout_cap_bytes` and `[worker].stderr_cap_bytes` are per-stream truncation points, not separate budgets. Both streams and child-server output are copied into one `runtime.log` whose retained total is the request's shared `max_log_bytes` (32 MiB); the two per-stream values are clamped together by that shared cap and never total 64 MiB. Once the log is full, further bytes are dropped and truncation is recorded.
- `[worker.egress].model_api_hosts` is a non-empty list of bare host names: no scheme, port, path, wildcard, IP literal, or empty entry. It is disjoint from every `[hosts.*].endpoint` host and from `[verification.egress].allowlist`; overlap is a load error. It is the only destination the Worker proxy may CONNECT to.
- `[verification].toolchain` and every `cache_paths` entry are absolute, present, and readable before the first dispatch. They are mounted read-only into the verification sandbox. A missing toolchain or cache is a preflight failure, never a download at verification time.
- `[verification.egress].allowlist` remains the installation default. The effective per-command set is this list plus the Work's immutable `verification_egress_hosts` declared at admission; it never includes a model-API host. A Work that needs an undeclared host is an admission gap, not a reason to open the network.
- All numeric keys are positive integers with the documented units. `0`, negative, fractional, and unit-suffixed values are load errors; `0` is never silently "unlimited". The outbox/reconcile values may not exceed the global per-tick bounds in the [execution loop](execution-loop.md#loop-model).
- Unknown keys anywhere in a v2 profile are refused, including an attempt to spell a mount path, credential variable name, or proxy host in an unmodelled location. Mounts and the proxy are profile-owned, not operator-injected.
- `[hosts.*]`, `[[hosts.*.repositories]]`, `[roles.*]`, capability floors, recovery, retention, selector, and intake retain their existing semantics. The host driver uses those bindings; it does not create a second host or credential table.

Compatibility:

- A v1 profile loads and keeps its recorded behavior. It does not gain v2 enforcement and must not be presented as namespace- or egress-enforced.
- A request or Work accepted under v1 keeps its binding, budgets, credential references, isolation profile id, and unresolved effects. A reload or a v2 config affects only requests not yet bound.
- Removing the v2 profile, toolchain, or driver stops new v2 dispatch; running requests keep their recorded binding only while their profile is still installed. Otherwise they are abandoned with a typed repair/unsupported reason and the work is re-issued; a running Invocation is never migrated or silently downgraded.

## Clean-install v0.2 profile

The [v0.2 installation and operation guide](v0.2.md) is the
operator-facing walkthrough. The sanitized, structurally valid starting
profile is [`docs/examples/v0.2/config.toml`](examples/v0.2/config.toml):
one host, one publication repository, publication/review/discussion
roles, one worker binding, and exactly one `[intake.selector]` binding.
It contains no credential values and no live identity.

The profile is not the only configuration file a running installation
reads — the single-TOML design above is the target, while the current
runtime keeps a second file. The store-backed commands (`submit`,
`run`, `intake`, …) load a JSON **installation record** — `--config`,
or `<db-stem>.config.json` beside `--db` — which carries the host
binding (`adapter`, `host_instance`, `repository`, `host_actors`,
`capabilities`, `contract_version` — all required at load), the actor
binding, `capacity`, `worker_argv`, `worker_isolation_profile`,
`workspace_root`, `allowed_effects`, and the
`budget`/`policy`/`retention`/`loop` overrides. The profile's
`[worker]` section is the declaration `check` stage 5 validates;
`worker_argv` in the installation record is the launch source
`taisan run` binds. The guide's
[two-file section](v0.2.md#two-files-the-profile-and-the-installation-record)
documents the reconciliation an operator performs; `check --probe`
requests the opt-in adapter capability stage but no probe backend is
wired into this runtime yet, so that stage fails closed
`capability.unknown` rather than fabricating a result.

The v0.2 intake keys and defaults are:

| Key | Default | Rule |
| --- | --- | --- |
| `[intake].enabled` | `false` | Automatic scheduled intake is off. The one-shot `taisan intake` verb still requires only a configured selector binding. |
| `[intake].interval_minutes` | `30` | Positive whole minutes on the internal scheduler grid. |
| `[intake].timeout_seconds` | `120` | Positive integer, at most `300`; the selector exchange deadline. |
| `[intake].pending_queue_limit` | `10` | Positive integer installation-wide pending-Work bound, applied to manual and Selector submissions alike. |
| `[intake.selector]` | absent | Exactly one binding when present; `enabled = true` requires it. |

The scheduler's fixed 60-second settings tick is internal to
`taisan run`. No cron, systemd, launchd, or other OS timer is required.
Reload, enable/disable, interval-change, adapter-swap, and no-catch-up
semantics are specified in the
[execution loop](execution-loop.md#intake-scheduling) and demonstrated
by the [v0.2 walkthrough](v0.2.md#operation-walkthrough). Changing only
the selector binding is a configuration-only adapter replacement; it
never rewrites accepted Work bindings.

## Credential references and secret hygiene

A `credential_ref` names a slot, not a secret. Resolution happens once per use at the adapter boundary; the resolved value stays inside the adapter process and is never persisted, logged, placed on argv, embedded in intents or receipts, or exported in evidence.

| Provider | Form | Validation | Resolution semantics |
| --- | --- | --- | --- |
| `env:` | `env:NAME` | Name is a valid environment identifier; the variable's existence is checked at preflight/dispatch without echoing the value | Reads the process environment of the adapter process |
| `file:` | `file:/absolute/path` | Absolute path; regular file; owner-only permissions (`0600`, or `0400`) verified before reading; symlinks resolved and rechecked | Reads the file's exact bytes; a permission failure is `credential.unresolvable`, never a weaker read |
| `command:` | `command:["argv",...]` | Non-empty argv; deterministic executable resolution (absolute path or pinned lookup); no shell | Executes the command in the adapter's trusted context and captures stdout; stderr and exit code bound the failure reason |

Failure classes at resolution: `credential.missing` (slot undefined), `credential.unresolvable` (provider failed — unset variable, unreadable file, nonzero command), `credential.expired` (provider or adapter reports expiry), `credential.actor_mismatch` (resolved identity differs from the bound actor). Every failure is typed and bounded for display; the secret value can never appear in a failure message.

## Roles and permission matrix

Checks at four boundaries — configuration load, admission, dispatch, and effect application. Capability support and authorization are evaluated separately (`supported`/`unsupported`/`unknown` × `allowed`/`denied`/`unknown`) per the [host contract](repository-host-contract.md#common-semantics-and-optional-capabilities).

| Boundary | Publication role | Review role | Discussion role | Reconciliation |
| --- | --- | --- | --- | --- |
| Config load | actor+credential_ref parse; required capability names valid; host/repo declared | same; reviewer identity recommended distinct | same | n/a |
| Admission | Work's required ops ⊆ supported∩allowed evidence for this actor | same for review ops | same for comment ops | read ops verified for every bound actor — reconciliation must survive lost write permission |
| Dispatch | fresh capability report re-checked; unavailable → defer with diagnostic | same | same | read capability re-checked; unknown read → `unknown`, never "no effect" |
| Effect apply | intent's operation re-checked at the apply boundary; expected prior-state guard enforced | same | same | `reconcile` uses the original binding's read permission only; never a new write |

Outcomes are typed: a required operation that is `unsupported` refuses admission; `denied` holds with an authorization failure and repair prerequisite; `unknown` leaves admission unaccepted or defers dispatch. No boundary treats "permission check skipped" as pass.

## Rotation, renewal, and binding preservation

- **Same-actor renewal**: a credential_ref updated to resolve to the *same verified actor* (token refresh, new secret file, provider change) is permitted. Existing Work, intents, and unresolved effects are unaffected; the next resolution picks up the new secret under the same actor identity.
- **Actor replacement**: binding a role to a different actor identity is a binding change. Existing Work and unresolved effects keep the original actor; the new actor applies to Work accepted after the change. An intent always records the actor it was authorized under.
- **Lost read permission**: reconciliation of prior unknown effects stays possible while read access survives; when reads fail, effects remain `unknown` and ownership is retained — never converted to failure, release, or a new action.
- **Removed adapter or host**: new Work cannot select it; existing Work retains its recorded binding and needs the old adapter for reconciliation — removing it surfaces `unavailable` evidence and blocks release.
- **Changed defaults**: apply to new Work only; they cannot silently retarget or silently downgrade an existing binding.
- **Unresolved effects**: always reconcile against the intent's original actor/credential binding. Credential rotation cannot reattribute, replay, or erase an unresolved action.

## Setup contract

Two boundaries, both auditable and non-destructive.

**`init`** scaffolds `config.toml` only from observed evidence: it may discover adapter-reported identities (host instance, repository stable id, actor slug) through explicit user-supplied pointers, but never fabricates identities, permissions, or capabilities. It writes only installation-owned files via the existing atomic publish, and never mutates shared user configuration (`~/.gitconfig`, shell rc files, external tools' state). A partial init leaves either no config or a complete one.

**`check`** runs an ordered read-only fail-closed preflight. Each stage names its failure and stops the sequence:

1. Parse `config.toml` (schema_version, unknown keys, structural validity)
2. Identity tuples: host/repository/actor scoping, allowlist coherence, role completeness
3. Credential references: provider shape valid, slots resolvable in *existence* (env present, file readable with `0600`, command exits zero) — values are never echoed
4. Paths: state/workspace/bodies/handoff roots exist or are creatable, writable, correctly nested; a v2 profile additionally requires the absolute provider-config path and every verification toolchain/cache path to be present and readable
5. Worker runtime: `worker.argv` resolves deterministically, `runtime_version` constraint matches the observed binary, and a v2 profile's runtime credential reference resolves in existence without echoing its value
6. Isolation primitives and operational profile: `bwrap` and the CONNECT-proxy endpoint are present, and (v2) the model-API host set, in-process host driver, read-only mount set, and verification toolchain can be resolved. A missing primitive refuses v2 dispatch; it never degrades to hygiene-only
7. Store: schema version recognized, no unknown/corrupt state
8. `--probe` only: read-only `capabilities()` calls for each bound actor and the pinned repository context; reported per operation and actor

`check` is read-only: it creates no Work, mutates no config, and touches no remote state beyond the opt-in probe. Its output is a structured per-stage report — absent config, malformed TOML, missing/expired/unresolvable credentials, and unwritable paths are each distinct results with the exact key/path named. A passing `check` proves declared prerequisites, not live namespace or egress enforcement; the operational negative probes in [v0.3 §14](v0.3.md#14-operational-launch-verification-and-host-composition) are separate evidence.

## Failure and repair semantics

Configuration, authentication, and environment failures follow the shared repair contract: a repair prerequisite names the failed prerequisite, the actor/action required, and the evidence that must change before resume. An unchanged broken configuration cannot create an automatic retry loop — resume requires fresh validated evidence, never a retry of the same failing read.

| Failure | Code | Repair actor / action | Evidence required before resume |
| --- | --- | --- | --- |
| Absent config file | `config.absent` | Operator runs `init` or supplies `--config` | Valid config loads end-to-end |
| Malformed/unknown keys | `config.invalid_key` | Operator edits config; key named in error | `check` stage 1–2 pass |
| Credential slot missing | `credential.missing` | Operator defines the ref | Ref parses; provider exists |
| Expired/denied credential | `credential.expired` / `credential.unresolvable` | Operator renews credential; `check` stage 3 passes | Fresh resolution succeeds; bound actor still matches |
| Lost permission on a required op | `capability.denied` | Operator re-grants or binds an authorized actor | Fresh capability evidence `allowed` |
| Unsupported required capability | `capability.unsupported` | Change required profile or adapter (new binding) | Cannot repair existing Work — new Work only |
| Source unavailable | `source.unavailable` | Transient: hold/retry within recovery bounds | Fresh authoritative read |
| Absent worker runtime | `runtime.missing` | Operator installs the pinned runtime | `check` stage 5 passes |
| Runtime version mismatch | `runtime.version_mismatch` | Upgrade runtime or update the pin deliberately | Observed version satisfies the pin |
| Missing `bwrap`, proxy, or model-API allowlist | `isolation.primitive_missing` | Install the approved primitive or bind a profile that has it; never weaken the profile | `check` stage 6 and the v0.3 negative probes pass |
| Missing verification toolchain/cache | `verification.toolchain_missing` | Operator installs the pinned toolchain/cache or updates the pin deliberately | `check` resolves the toolchain and cache read-only |
| In-process host driver unavailable or wrong mode | `host_driver.mode_unsupported` | Choose the supported `in_process` driver; a separate driver needs its own owner decision | `check` stage 6 passes and the driver constructs |
| Changed actor binding mid-Work | `binding.actor_mismatch` | n/a — original binding retained | Original actor reconciles existing effects; new actor serves new Work only |
| Broken config left unchanged | (no code — no retry) | n/a | Resume requires the failed prerequisite's evidence to have changed |

Repair evidence is always fresh and validated: the exact failed prerequisite acquires new evidence, and resume rechecks WorkVersion, binding, remaining budgets, and unresolved ownership (per #51's changed-evidence rules).

## Evidence validity for configuration items

The shared four-state contract applies to configuration, credential, and capability evidence:

| State | Examples in this scope | Effect |
| --- | --- | --- |
| `absent` | No config file; a required role never declared; a capability report never produced | Dependent operation cannot proceed; absence is never an empty pass |
| `present` + valid | Config parses with exact schema_version; capability evidence matches the bound host/repository/actor/operation | Satisfies the boundary it was produced for |
| `present` + invalid | Malformed TOML, stale capability evidence for a different revision, mismatched actor, credential resolving to a different identity | Retained with its reason; cannot satisfy; demands repair, never substitution |
| `unavailable` | Credential provider unreachable, capability probe timed out, adapter removed | Typed reason retained; `unknown` — never treated as denied or as supported |

## Literal examples

Values are illustrative, not executed evidence.

| Scenario | Result |
| --- | --- |
| `credential_ref = "file:~/token.txt"` | Load error: `file:` requires an absolute path and `~` is not expanded |
| `file:` target is `0644` | `credential.unresolvable` at check stage 3; secret never read under weak permissions |
| Token rotated in place, same actor slug verified by probe | Existing Work and unresolved intents unaffected; next apply resolves the new secret |
| Reviewer `credential_ref` now resolves to a different actor | `credential.actor_mismatch`; existing Work keeps its original reviewer binding — new binding serves new Work only |
| `check` on a host with no `bwrap` | Stage 6 fails `runtime.missing` with the isolation primitive named; earlier stages' passes are still reported |
| `check --probe` reports `push_ref: denied` for the publication actor | Admission of Work requiring `push_ref` will refuse at the admission boundary; nothing was dispatched |
| Config adds `comment_write` to review requirements mid-Work | Existing Work's saved binding/policy unchanged; the new floor applies to Work accepted afterward |
| `--probe` unreachable | `capability.unknown`, not `denied`; admission/dispatch hold rather than fail-closed to a wrong negative |
| Unresolved push intent, then token revoked and replaced by a different actor's token | Reconciliation uses the intent's original actor; the replacement actor does not reattribute or replay it |
| `[worker].provider_config` names a path containing a token | Load/check error; provider config is non-secret and the credential belongs only in its `credential_ref` |
| `[worker.egress].model_api_hosts` overlaps a host endpoint or verification allowlist | Load error; the model and verification allowlists are disjoint and a wildcard is refused |
| v2 profile omits `bwrap`, proxy, toolchain, or `mode = "in_process"` | Load/preflight refuses v2 dispatch; no fallback to a v1 profile or hygiene-only launch |
| v1 profile loads after the v2 revision is introduced | Loads unchanged; its recorded behavior is retained and no v2 enforcement is claimed |
| Config edit while a v2 request is running | The running request keeps its mounts, credentials, driver, and budgets; only new requests bind the edit |

## Planned verification

Named checks for the configuration contract suite (CF). Planned contract scenarios, not executed results.

| Check | Scenario | Required result |
| --- | --- | --- |
| CF-01 | Config with unknown key / missing `schema_version` / newer version | Load refused, key named, file untouched |
| CF-02 | `0`, negative, unknown-unit limit values | Load errors naming the key; `unlimited` honored only where its document permits |
| CF-03 | `env:`/`file:`/`command:` refs — present, absent, wrong-permission file, nonzero command | Each distinct typed failure; no value in any output |
| CF-04 | Secret in argv/evidence/report/logs | Static scan plus seeded-secret test: zero occurrences |
| CF-05 | Role missing `actor`/`credential_ref`; actor on undeclared host | Load error naming the role |
| CF-06 | Required capability `unsupported`/`denied`/`unknown` at admission | Refuse / hold-with-repair / unaccepted — never silent pass |
| CF-07 | Capability report changes between admission and dispatch | Dispatch defers with diagnostic; saved binding intact |
| CF-08 | Same-actor renewal mid-Work | Unresolved effects reconcile under original actor; new secret resolves |
| CF-09 | Actor replacement in config | Existing Work retains original binding; new Work uses new actor; unresolved intent never reattributes |
| CF-10 | Lost read permission during reconciliation | Effect stays `unknown`, ownership retained |
| CF-11 | Adapter/host removed from config | Existing Work's binding retained; reconciliation reports `unavailable`; new Work cannot bind it |
| CF-12 | `init` on a host missing the runtime | Scaffold incomplete-failure report; no partial config published |
| CF-13 | `check` stage ordering | A stage failure stops later stages; report names the failed stage and key/path |
| CF-14 | `check` is read-only | Store, config, remote state byte-identical afterward; probe is opt-in only |
| CF-15 | Unchanged broken config retried | Same failure, no automatic retry loop; resume requires changed evidence |
| CF-16 | `TAISAN_CONFIG`/`TAISAN_STATE_DIR` precedence | Flag > env > XDG default; ambient cwd never used |
| CF-17 | Workspace/state path nesting violation | Refused at check stage 4 |
| CF-18 | `use = "source"` repo targeted by a publication intent | Refused structurally before any adapter call |
| CF-19 | Model/runtime version pin vs. observed binary | Mismatch is `runtime.version_mismatch`; loaded config records the pin actually verified |
| CF-20 | Credential resolving to wrong actor | `credential.actor_mismatch` — never silently rebinds |
| CF-21 | Recovery/retention key invalid in the same file | Same load-error path as other keys; one schema, one validator |
| CF-22 | Deterministic runtime resolution | `worker.argv` resolution ignores ambient PATH surprises; explicit overrides only |
| CF-23 | Sanitized example parses | The documented example loads under the same validator |
| CF-24 | Interrupted `init` publish | Either no config or a complete config exists; scratch residue never loads |
| CF-25 | v2 operational profile missing a required key or containing a non-`in_process` driver | Load refused naming the key; no partial profile and no v1 fallback |
| CF-26 | v2 provider config contains a secret, is workspace-supplied, or is not read-only | Refused/ignored; only the supervisor-rendered read-only config is used |
| CF-27 | v2 model-API host set contains a URL, port, wildcard, empty entry, host overlap, or verification-host overlap | Load error; no wildcard-like grant reaches the proxy |
| CF-28 | v2 runtime credential is absent, unresolvable, or resolves to a host token | Environment repair; dispatch refused; the value never appears in output |
| CF-29 | v2 verification toolchain/cache missing or writable | Preflight fails closed; verification never installs dependencies from an open network |
| CF-30 | v1 and v2 profiles coexist across new and running requests | New requests bind v2; running requests keep recorded v1/v2 bindings; no silent upgrade, downgrade, or retarget |

## Requirement mapping

All 51 inventory targets owned by #53 are mapped in [installation requirements](installation-requirements.md). Source automation mechanisms are omitted and their retained obligations re-modelled here, recorded per requirement.

## Six design aspects

| Aspect | Risk without control | Control in this design | Open / deferred |
| --- | --- | --- | --- |
| State transitions | A config change could silently retarget live Work or an unresolved effect | Saved binding is immutable per Work; capability checks repeat at every boundary; rotation rules separate credential renewal from actor replacement | — |
| Failure semantics | A broken config could flap in a retry loop, or an expired secret could masquerade as a decision | Typed failure codes + repair prerequisites; resume requires changed evidence; unknown stays unknown | Exact CLI output envelopes are in the [execution loop](execution-loop.md#cli-contract) |
| Trust boundaries | Secrets could leak into argv/logs/evidence; a worker could inherit host credentials | Refs-only in config; provider resolution at the adapter boundary; worker environment carries no host credential (with #52's isolation) | `command:` provider executes trusted config — its argv is config, not Core input |
| Identity invariants | Actor/credential could be silently re-bound; capability evidence could bind the wrong host/repository | Actor identity verified against binding at resolution/probe; capability evidence carries exact context; replaced actors serve new Work only | — |
| Information preservation | Config edits could erase provenance of who authorized what under which policy | Effective policy + binding saved per Work; receipts record the observed actor; schema versioning refuses silent reinterpretation | — |
| Resource bounds | Preflight could hang on a live probe; file reads could be unbounded | Probe is opt-in and bounded; `check` is ordered fail-closed; bounded report output | Probe timeouts inherit recovery transport bounds |

## Implementation-plan mapping and non-claims

- **U2**: `config.toml` schema and validator, credential-reference resolver, role/capability checks, `init`/`check` preflight, installation layout, secret-hygiene tests.
- **U1**: store-side persistence of saved bindings/effective policy (already designed under #50/#51).
- **U3**: the GitHub adapter implements `capabilities()` and per-operation authorization evaluation for the GitHub permission model.
- **U5**: dispatch-time revalidation wiring into the CLI run loop.

This document claims no installed configuration, no resolved credential, no live probe result, and no runtime implementation. Deferred items: GitHub App authentication setup (token-first is the v0.1 path; App support is a later adapter unit), a `command:` provider hardening guide beyond the contract above, and any config hot-reload protocol for the running loop ([execution loop](execution-loop.md) scope).
