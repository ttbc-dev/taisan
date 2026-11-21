# Public reference adapters

Status: documentation of the two public reference adapters a third
party can install and run without any private piece — the reference
Selector `public-issue-selector` and the reference Worker
`taisan-worker-claude`. Both are independently replaceable ports
([adapter boundaries](adapter-boundaries.md)): each is selected by
configuration alone and swapping either requires no Core change.

**Evidence boundary.** Everything on this page is verified by
automated contract/wire tests (`cargo test --workspace`) against
scripted inputs and a fake runtime executable — never live evidence.
No live model call, live source read, or live host write was performed
for this document; owner-run live acceptance remains a separate
activity tracked outside this page. Where a claim can only hold in a
live environment (a real `claude` binary, ambient `gh` auth, real
credentials), it is stated as configuration intent, not observed
behavior.

## Building and installing

```sh
cargo build --release -p workloop-selector -p taisan-worker-claude
# binaries:
#   target/release/public-issue-selector
#   target/release/taisan-worker-claude
```

Or install onto a `PATH` directory:

```sh
cargo install --path crates/workloop-selector --bin public-issue-selector
cargo install --path crates/taisan-worker-claude --bin taisan-worker-claude
```

No further build-time setup exists: neither adapter embeds a private
service, endpoint, or credential. The Worker adapter additionally
requires its runtime — the Claude Code CLI (`claude`) — installed and
resolvable at dispatch time, plus a runtime credential staged where
the installation record points (below).

## Reference Selector: `public-issue-selector`

### Entry point and transport

```text
public-issue-selector select [--sources <file.json>] \
    [--label <name>]... [--github-endpoint <id>]...
```

The adapter speaks the stateless external Selector protocol
([selector protocol](selector-protocol.md)): one JSON request on
stdin, one JSON response on stdout, exit `0` on contract-complete
responses. It is stateless — every invocation re-reads the full
requested scope and keeps no cursor and no acceptance ledger.

### Source-read authentication and scope

Two source backends:

- **`--sources <file.json>`** — an offline file of source facts in the
  protocol's collected-source shape. Fully deterministic; used by the
  contract suite and for local trials.
- **`gh api` (default)** — live reads against `github.com` via the
  GitHub CLI's own transport. Authentication is ambient `gh`
  authentication only: the adapter passes `HOME` through and never
  reads, forwards, or synthesizes `GH_TOKEN`/`GITHUB_TOKEN`. The
  operator resolves source-read auth with `gh auth login`; the adapter
  never manages credentials.

Repository scope is **fail-closed**: a host instance only becomes
legible when explicitly declared through `--github-endpoint`. Requests
naming undeclared host instances are refused; there is no implicit
scope.

Collection is bounded: page size 100, at most 10 pages per repository,
at most 1,000 Issues per repository, at most 256 source calls per
invocation. A bound hit is reported, never silently absorbed.

### Selection policy

The reference policy (`crates/workloop-selector/src/reference.rs`) is
deliberately minimal and deterministic:

- only Issues inside the requested repository scope;
- request exclusions are always removed — an Issue owned by unfinished
  Work is never re-proposed;
- only open Issues;
- configured `--label` values are an all-of filter;
- display number `0` and invalid/blank/oversized titles are rejected
  and counted in the skipped accounting;
- deterministic ranking by creation timestamp, then display number,
  then scoped identity;
- exactly one proposal per eligible Issue (no grouping), up to
  `max_candidates`;
- over-limit candidates are reported as `truncated`, never dropped
  silently;
- an empty selection produces **no proposals** with typed accounting
  for every skipped fact — an empty response is a contract-complete
  answer, not an error.

### Task defaults, exclusions, and empty results

The adapter invents no task semantics. `task.intent` is the Issue
title verbatim; `task.constraints`, `task.verification_argv`, and
`required_permissions` are copied verbatim from the request — which
the intake scheduler builds from the profile's `[intake.selector]`
`task_constraints`, `verification_argv`, and `required_permissions`
tables. Configuring the intended checks and permissions is a profile
decision; the adapter only echoes.

Exclusions flow the other way: the intake scheduler computes the live
source-owner set (Issues owned by unfinished Work) and passes it in
the request; the adapter removes those Issues. An empty or fully
excluded scope yields `proposals: []` plus the skipped/truncated
accounting — the scheduler treats this as a quiet tick, not a failure.

### Wiring

```toml
[intake.selector]
adapter = "public-issue-selector"
selector_version = 1
policy_revision = 1
argv = ["public-issue-selector", "select", "--github-endpoint", "https://api.github.com"]
# optional task defaults echoed into every proposal:
task_constraints = ["keep the change small"]
verification_argv = ["cargo", "test", "--workspace"]
required_permissions = []
```

Interchange is exercised: the shared contract suite runs the same
scenarios against this executable and an independently implemented
fixture adapter, including a configuration-only argv swap through the
real intake path — see
[the interchange evidence](evidence/selector-adapter-interchange-217.md).

## Reference Worker: `taisan-worker-claude`

### What it is

A thin taisan-owned adapter that drives the **Claude Code CLI** in
headless mode as the coding-agent runtime. It implements the exact
argv-v2 transport of the Worker protocol
([worker protocol](worker-protocol.md), [v0.3 wire contract](v0.3.md)):

```text
taisan-worker-claude [flags] describe
taisan-worker-claude [flags] run --request <path> --out <dir>
```

`describe` is effect-free and bounded: it probes the runtime's
`--version` under a short deadline and prints one
`taisan.worker.declaration/1` body. `run` reads one immutable
`taisan.worker.request/2` body, spawns the runtime once inside the
granted workspace, captures a bounded `runtime.log`, and writes one
bounded `taisan.worker.report/2` `report.json`. Nothing else is
produced; there is no stdin protocol, no shell fallback, and no second
transport.

### Flags (bound through `worker_argv`)

| Flag | Default | Meaning |
| --- | --- | --- |
| `--runtime <argv0>` | `claude` | The runtime executable to spawn |
| `--runtime-arg <arg>` | — | Extra leading runtime argv (repeatable; e.g. a wrapper's flags) |
| `--runtime-auth <path>` | `/taisan/runtime-auth` | File containing the runtime credential |
| `--provider-config <path>` | `/taisan/provider-config` | Supervisor-validated JSON provider config |

### Declaration

```json
{
  "format_version": "taisan.worker.declaration/1",
  "adapter": "taisan-worker-claude",
  "adapter_version": "<crate version>",
  "runtime": {"product": "claude-code", "version": "<observed --version>"},
  "protocol_versions": ["taisan.worker.request/2", "taisan.worker.report/2"],
  "roles": ["coding", "fix", "invariant_check", "review"],
  "capabilities": ["invocation_transport.argv-v2", "process_control",
                   "report_json", "workspace_read_only", "workspace_write"],
  "resume": "unsupported",
  "checkpoints": "workspace_commits",
  "runtime_artifact_store": null,
  "local_retries": "runtime_internal"
}
```

`resume` is honestly `unsupported`: one Invocation is one runtime
process tree, and continuations arrive as new Invocations carrying
retained workspace-commit/artifact/report references — never a runtime
session handle.

### How a `run` maps onto the runtime

- **Workspace.** The runtime's working directory is the request's
  granted workspace mount (`/workspace` inside containment; the bound
  path otherwise). `coding`/`fix` requests carry `read_only: false`
  and may edit and commit locally; `invariant_check`/`review` carry
  `read_only: true` and the runtime additionally runs in plan
  permission mode so edit tools are denied even outside containment.
- **Runtime invocation.** `claude -p <prompt>
  --output-format stream-json --max-turns <budget.max_turns>
  --strict-mcp-config --mcp-config <empty> --permission-mode
  <acceptEdits|plan>` — writers get `acceptEdits` plus
  `--allowedTools "Bash(git *)"` (local commits only); a configured
  `model` is forwarded as `--model`.
- **Prompt construction.** The shim translates the request's retained
  inputs into one bounded prompt: the goal body, the ordered Issue
  snapshots, constraints, and declared `verification_argv` for
  writers; the artifact reference plus the parsed invariant set for
  `invariant_check`; the diff target plus prior-round context from the
  materialized review source for `review`; the predecessor and origin
  (sealed review findings or failed evidence) for `fix`.
- **Result extraction.** The stream's terminal `result` event carries
  the runtime's final text. Reader roles must end with one fenced
  ```` ```taisan-result ```` block containing their claim
  (`{"findings": [...]}` or `{"invariant_items": [...]}`); members are
  validated against the closed member shapes before entering the
  report — a malformed member or missing block is a typed `failed`
  report, never a silently dropped member and never an empty success.
- **Claims are mechanical.** `claimed_head` is `git rev-parse HEAD`
  read by the shim, `claimed_checks` are the declared
  `verification_argv` commands **run by the shim itself** (argv, exit
  code, captured-output digest — never the model's word). The runtime
  cannot upgrade its own report.
- **Deadline and lifecycle.** The runtime tree gets its own process
  group; at the earlier of `budget.max_runtime_seconds` and
  `budget.deadline_unix_seconds` the shim SIGTERMs, then SIGKILLs the
  group after a short grace, and reports `limit_reached`. The log cap
  (`budget.max_log_bytes`) likewise stops the tree with
  `output_limit_reached`. The supervisor's descendant snapshot still
  covers the tree on an external stop.
- **Report.** `report.json` echoes the exact request binding
  (`request_id`, `invocation_id`, `request_digest`, `role`), carries a
  verified `report_digest`, and stays inside
  `budget.max_report_bytes`. An exit before the report is written is
  the contract's `absent` class; a request that fails the closed
  parse or digest check produces no process and no report.

### Runtime-only authentication

The runtime credential is resolved **by reference** and staged at the
sandbox `/taisan/runtime-auth` mount (or the path given by
`--runtime-auth` in uncontained launches). The shim reads the file and
sets it as the runtime child's `ANTHROPIC_API_KEY` — on that child's
environment only. It is never set on the shim's own process, so the
shim-run verification commands and the report/log never observe it.
`GH_TOKEN`/`GITHUB_TOKEN` are stripped from the child's environment
entirely: the Worker has no GitHub mutation authority, no inherited
credential helper, and no publication credential by construction.

`--provider-config` points at the supervisor-validated, read-only
provider JSON; the adapter reads `model`, `endpoint` (→
`ANTHROPIC_BASE_URL`), and `proxy_endpoint` (→ `HTTPS_PROXY`). The
credential can never be embedded in provider config.

### What the Worker can never do

The report is evidence only. The adapter cannot publish, push, comment,
request retries, change Work state, or waive verification — durable
handoff, supervisor verification, enabled gates, and host-effect
reconciliation remain Core- and host-owned. A `success` report is a
claim that still stands between the supervisor's own
`VerificationResult` and any Outcome.

### Wiring

The launch binding is the installation record (`<store>.config.json`),
with the profile's `[worker]` section declaring what `taisan check`
validates:

```toml
[worker]
argv = ["taisan-worker-claude", "--runtime", "claude"]
isolation_profile = "taisan-reference-v0.1"
runtime_version = ">=1.0.0"
max_turns = 200
```

```json
{
  "worker_argv": ["taisan-worker-claude", "--runtime", "claude"],
  "worker_isolation_profile": "taisan-reference-v0.1",
  "worker_credential_ref": "env:TAISAN_WORKER_TOKEN",
  "worker_provider_config": "/home/operator/.config/taisan/worker-provider.json",
  "worker_model_api_hosts": ["api.anthropic.com"]
}
```

`worker_credential_ref` resolves to the staged runtime credential
(never a value in the record); `worker_provider_config` is the staged
provider JSON; `worker_model_api_hosts` allowlists the model API
egress the supervisor's CONNECT proxy serves. On the production
profile (`taisan-reference-v0.1`, Linux + `bwrap`) the workspace,
request, input, runtime-auth, provider-config, and proxy mount points
are the sandbox paths above; the read-only grant additionally mounts
the workspace read-only for assessment roles.

### Swapping either adapter

Both ports are bound by argv in configuration:

- Selector: `[intake.selector].argv` — a conforming replacement only
  has to speak the request/response protocol on stdio.
- Worker: `worker_argv` in the installation record — a conforming
  replacement only has to answer `describe` and `run` on argv and emit
  the `/2` envelopes. `--runtime` substitutes a different runtime
  executable behind the same declaration shape for runtimes with a
  compatible headless interface.

Neither swap touches a Core type, a state transition, or an Outcome
rule. The two adapters do not share code, configuration, or
credentials: they are exercised independently by the contract suites.

## Composed configuration example

[`examples/reference-adapters/`](examples/reference-adapters/README.md)
contains a complete standalone example binding both public adapters
through the advertised workflow — real Issue intake → Work → Coding →
verification → publication → review finding → Fix → republish →
reassessment — with the operator steps, the configuration-only
evidence each stage produces, and the explicit boundary between what
the example proves by configuration and what only a live run can
prove.
