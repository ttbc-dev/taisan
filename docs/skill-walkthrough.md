# taisan walkthrough — setup, install, submit, inspect, Attention/decision, recovery

Status: U7.4 delivery documentation. This walkthrough documents the
operator sequence **setup → install → submit → inspect →
Attention/decision → recovery** against one isolated fixture
installation, and every step section below carries **verbatim**
captured CLI output from a single verification run of this checkout:
the JSON envelopes on stdout and the exit codes are captured exactly as
the `taisan` binary printed them, and the stderr diagnostic lines are
shown wherever the CLI emits one (diagnostics never corrupt stdout).

Claims boundary: this is a **fixture/contract** walkthrough — the real
CLI binary against a real SQLite store with the default fixture
installation. It supports contract claims about this repository only.
It is not live U8 evidence: no live host, Worker, or E2E scenario is
run or claimed here, and any live claim requires separately authorized
live evidence recorded under `docs/evidence/u8*.md` and labeled `live`.
TUI and MCP are not prerequisites: nothing below needs them. The
exercised versions, configuration, and per-row `fixture`/`live` labels
are retained in the [U7.4 verification evidence](evidence/u7.4.md).

Capture notes:

- `REPO=/runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607` is the checkout; `W=$REPO/target/u74-capture/walk` is
  the walkthrough installation directory and
  `CLEAN=$REPO/target/u74-capture/clean` is the pristine directory used
  for the clean skill install (both git-ignored scratch directories on
  the verification host — substitute your own paths). The absolute
  paths inside the captured output are those capture-host paths.
- Command lines are shown with the quoting an operator would type; the
  output under each command is the captured transcript.
- The CLI is on `PATH` as `taisan` after
  `cargo build --workspace` (or `cargo install --path
  crates/workloop-cli --bin taisan`); every `taisan …` command below
  was run from this checkout's build.
- The supported agent for the captured install is **OpenCode**
  (`-a opencode`); the skill itself is agent-agnostic
  (`skills/taisan/SKILL.md`) and installs into any supported agent's
  skill directory through the same command.

## Step 1 — setup

Setup scaffolds and validates the installation profile. The
installation resolves its config and state through flag → `TAISAN_*`
environment → XDG precedence, so the walkthrough exports both to the
walkthrough directory. The credential is supplied as a credential
reference (`env:TAISAN_TEST_TOKEN`) — never as a value in argv — and
`init` refuses to publish without an observed identity. The walkthrough
environment (shell setup for the commands below):

```text
$ mkdir -p "$W" "$W/state"
$ export TAISAN_CONFIG="$W/config.toml" TAISAN_STATE_DIR="$W/state"
$ export TAISAN_TEST_TOKEN=walkthrough-credential-value
```

```text
$ taisan init --endpoint https://github.com --repo 0123456789:publication --role publication:taisan-publisher:env:TAISAN_TEST_TOKEN --observe publication:taisan-publisher@https://github.com/0123456789 --egress crates.io
{
  "schema_version": "1",
  "status": "accepted",
  "command": null,
  "work_id": null,
  "execution_id": null,
  "result": {
    "egress_allowlist": [
      "crates.io"
    ],
    "identity_evidence": [
      "publication"
    ],
    "kind": "init",
    "path": "/runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/config.toml",
    "roles": [
      "publication"
    ]
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

`init` published the profile at the resolved path with the observed
identity evidence. `check` is the ordered offline preflight: seven
stages pass and the opt-in probe stage reports `not_run`.

```text
$ taisan check
{
  "schema_version": "1",
  "status": "accepted",
  "command": null,
  "work_id": null,
  "execution_id": null,
  "result": {
    "kind": "check",
    "probe_operations": [],
    "remote_writes": 0,
    "stages": [
      {
        "diagnostics": [],
        "stage": "config_parse",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "identity_tuples",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "credential_refs",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "paths",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "worker_runtime",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "isolation_primitives",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "store",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "probe_capabilities",
        "status": "not_run"
      }
    ],
    "success": true
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

`check --probe` is opt-in and fails closed — no probe backend is wired,
so the probe stage fails with `capability.unknown` at config class
(exit 3) rather than fabricating a pass. A plain `check` never probes.

```text
$ taisan check --probe
{
  "schema_version": "1",
  "status": "error",
  "command": null,
  "work_id": null,
  "execution_id": null,
  "result": {
    "kind": "check",
    "probe_operations": [],
    "remote_writes": 0,
    "stages": [
      {
        "diagnostics": [],
        "stage": "config_parse",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "identity_tuples",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "credential_refs",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "paths",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "worker_runtime",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "isolation_primitives",
        "status": "pass"
      },
      {
        "diagnostics": [],
        "stage": "store",
        "status": "pass"
      },
      {
        "diagnostics": [
          {
            "code": "capability.unknown",
            "key": "probe",
            "message": "--probe was requested but no adapter probe backend is wired for this installation — probe reads require the host adapter; nothing was probed and nothing was written"
          }
        ],
        "stage": "probe_capabilities",
        "status": "fail"
      }
    ],
    "success": false
  },
  "reason": {
    "code": "config",
    "message": "check failed at probe_capabilities: capability.unknown — --probe was requested but no adapter probe backend is wired for this installation — probe reads require the host adapter; nothing was probed and nothing was written"
  },
  "evidence_refs": []
}
--- stderr ---
taisan: config: check failed at probe_capabilities: capability.unknown — --probe was requested but no adapter probe backend is wired for this installation — probe reads require the host adapter; nothing was probed and nothing was written
$ echo $?
3
```

## Step 2 — install

"Clean" means the shipped `skills/taisan/SKILL.md` is installed into an
environment with **no prior taisan config or state** (`$CLEAN` is
empty) and then invoked once end-to-end. Two documented install paths
produce the same destination:

- **Path A (captured below):** `npx skills add <path-to-skill> -a
  <agent> -y --copy` — from this checkout with a local path, or from
  the registry as `npx skills add ttbc-dev/taisan` (the form named in
  the skill's frontmatter).
- **Path B (offline equivalent):** copy the same shipped file to the
  same destination —

```text
$ mkdir -p .agents/skills/taisan
$ cp "$REPO/skills/taisan/SKILL.md" .agents/skills/taisan/SKILL.md
```

Path A, captured against the shipped file in `$CLEAN`:

```text
$ mkdir -p /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/clean && cd /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/clean
$ npx -y skills add /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/skills/taisan -a opencode -y --copy --json
[
  {
    "name": "taisan",
    "status": "installed",
    "source": "/runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/skills/taisan",
    "ref": null,
    "hash": "35135bf6a783ce23dac1e908856a56d7981db8b183835046cfdc6fb1d48170f5",
    "path": "/runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/clean/.agents/skills/taisan",
    "scope": "project",
    "agents": [
      "OpenCode"
    ],
    "mode": "copy",
    "security": null
  }
]
exit=0
--- stderr ---

│
●   opencode  Agent detected — installing non-interactively
│
●  Skill: taisan
│
│  Operate the taisan CLI (execution loop) for an agentic workflow — submit Work, inspect recorded state and Attention, intervene on eligible Executions, judge review findings, and recover lost command responses. Install with `npx skills add ttbc-dev/taisan`.

│
◇  Installation Summary ────╮
│                           │
│  ./.agents/skills/taisan  │
│    universal: OpenCode    │
│                           │
├───────────────────────────╯

```

The install transcript names the source, the destination, the content
hash, the scope (`project`), and the agent (`OpenCode`). Discovery —
the actual skill location the agent loads:

```text
$ ls -la /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/clean/.agents/skills/taisan
total 2
drwxr-xr-x 2 worker worker    3 Nov 15 10:36 .
drwxr-xr-x 3 worker worker    3 Nov 15 10:36 ..
-rw-r--r-- 1 worker worker 9169 Nov 15 10:36 SKILL.md
```

One end-to-end invocation from the installed skill: the installed
document's own `submit` block, run against the pristine environment
with the full sample payload the requirements name — a sample issue
(`--issue` with its authoritative revision and digest, which the CLI
never fabricates), intent, constraints, and verification. The
installation record and store are created implicitly on first use —
nothing existed in `$CLEAN` before this command:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/clean/workloop.db submit --intent "Clean-install end-to-end invocation of the shipped skill" --constraint "Do not change the JSON envelope contract" --verification "cargo test --workspace" --issue 0123456789:42 --issue-revision 2026-11-15T10:36:12Z --issue-digest 6f1c9f94a1f9f0d42a97b21b81b1e47f08d68635673b26d852929e2b04b7d8c1 --idempotency-key clean-install-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": "cmd-1",
    "kind": "submit",
    "actor": "human:actor-18e8319afd189fd4-4463",
    "idempotency_key": "clean-install-1"
  },
  "work_id": "w-1",
  "execution_id": "e-1",
  "result": {
    "execution_id": "e-1",
    "kind": "work_created"
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

The remaining steps operate the configured installation in `$W` with
the same installed skill instructions; the direct `taisan …` commands
shown throughout are the identical calls the skill delegates to.

## Step 3 — submit

Submit queues durable Work against the configured installation. The
skill's submit block and the direct CLI are the same invocation; the
idempotency key is retained by the caller for replay. **Accepted means
durably queued — not completed, not necessarily started.**

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db submit --intent "Walkthrough: add a --version flag to the taisan CLI" --constraint "Do not change the JSON envelope contract" --verification "cargo test --workspace" --idempotency-key walkthrough-submit-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": "cmd-1",
    "kind": "submit",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-submit-1"
  },
  "work_id": "w-1",
  "execution_id": "e-1",
  "result": {
    "execution_id": "e-1",
    "kind": "work_created"
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

An identical replay (same key, same payload) returns the recorded
decision bound to the original identity — the same `cmd-1`, the same
`w-1`/`e-1`, and no second Work:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db submit --intent "Walkthrough: add a --version flag to the taisan CLI" --constraint "Do not change the JSON envelope contract" --verification "cargo test --workspace" --idempotency-key walkthrough-submit-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": "cmd-1",
    "kind": "submit",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-submit-1"
  },
  "work_id": "w-1",
  "execution_id": "e-1",
  "result": {
    "execution_id": "e-1",
    "kind": "work_created"
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

## Step 4 — inspect

The inspect set reads recorded state with canonical identities. A
not-found item would be `accepted` with `result: null`; an unreadable
store is `error` at exit 4; unavailable sources ride inside an
`accepted` envelope in `sources[].availability` — never translated into
an empty workload.

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db work-list
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "work_list",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": null,
  "execution_id": null,
  "result": {
    "items": [
      {
        "attention_count": 0,
        "current_execution_id": "e-1",
        "evidence": [],
        "outcome": null,
        "phase": "queued",
        "version": 1,
        "work_id": "w-1"
      }
    ],
    "next_cursor": null
  },
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 3
}
$ echo $?
0
```

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db work-detail --work w-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "work_detail",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": "w-1",
  "execution_id": null,
  "result": {
    "attention": [],
    "current_change": null,
    "current_execution_id": "e-1",
    "current_round_id": null,
    "evidence": [],
    "executions": [
      {
        "execution_id": "e-1",
        "invocation_ids": [],
        "outcome": null,
        "ownership": "confirmed",
        "state": "queued"
      }
    ],
    "findings": [],
    "judged": [],
    "outcome": null,
    "version": 1,
    "work_id": "w-1"
  },
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 3
}
$ echo $?
0
```

Attention is a Query: on this fresh installation nothing is derivable
yet (queue/CI waits are not Attention), and the empty set is a
completed query, not an error.

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db attention
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "attention",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": null,
  "execution_id": null,
  "result": [],
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 3
}
$ echo $?
0
```

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db capacity
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "capacity",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": null,
  "execution_id": null,
  "result": {
    "deferred": [],
    "holders": [],
    "limit": 1,
    "reservations": [],
    "used": 0
  },
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 3
}
$ echo $?
0
```

## Step 5 — Attention/decision

Re-query the Work's Attention before any decision — there is still
nothing derivable, so no `acknowledge` can be validly issued yet:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db attention --work w-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "attention",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": "w-1",
  "execution_id": null,
  "result": [],
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 3
}
$ echo $?
0
```

A decision with a stale precondition is a recorded rejection at exit 1,
never silently retargeted:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db cancel --work w-1 --expected-version 99 --idempotency-key walkthrough-stale-1
{
  "schema_version": "1",
  "status": "rejected",
  "command": {
    "id": "cmd-2",
    "kind": "cancel",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-stale-1"
  },
  "work_id": "w-1",
  "execution_id": null,
  "result": null,
  "reason": {
    "code": "stale_work_version",
    "message": "stale_work_version"
  },
  "evidence_refs": []
}
--- stderr ---
taisan: stale_work_version: stale_work_version
$ echo $?
1
```

Acknowledgement applies only to a currently derivable Attention
version; naming one the state does not derive is `ineligible`:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db acknowledge --work w-1 --expected-version 1 --attention att:w-1:paused_judgment:fixture:0 --idempotency-key walkthrough-ack-1
{
  "schema_version": "1",
  "status": "rejected",
  "command": {
    "id": "cmd-3",
    "kind": "acknowledge",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-ack-1"
  },
  "work_id": "w-1",
  "execution_id": null,
  "result": null,
  "reason": {
    "code": "ineligible",
    "message": "attention_id att:w-1:paused_judgment:fixture:0 does not name a currently derivable Attention version"
  },
  "evidence_refs": [
    "attention_id att:w-1:paused_judgment:fixture:0 does not name a currently derivable Attention version"
  ]
}
--- stderr ---
taisan: ineligible: attention_id att:w-1:paused_judgment:fixture:0 does not name a currently derivable Attention version
$ echo $?
1
```

A judgment decision without an observed change target is a
`source_unavailable` rejection at exit 5 — unavailable evidence stays
visible as a refusal, never as an accepted or empty judgment:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db judge-findings --work w-1 --expected-version 1 --head 0000000000000000000000000000000000000000 --decision occ-1:fix --idempotency-key walkthrough-judge-1
{
  "schema_version": "1",
  "status": "rejected",
  "command": {
    "id": "cmd-4",
    "kind": "judge_findings",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-judge-1"
  },
  "work_id": "w-1",
  "execution_id": null,
  "result": null,
  "reason": {
    "code": "source_unavailable",
    "message": "no observed change target for w-1"
  },
  "evidence_refs": [
    "no observed change target for w-1"
  ]
}
--- stderr ---
taisan: source_unavailable: no observed change target for w-1
$ echo $?
5
```

The skill's rules apply on every one of these: read `reason.code`,
surface it, and never bump `--expected-version`, swap a `--head`, or
mint a fresh key to make a rejection pass.

## Step 6 — recovery

Lost-response recovery: the submit below commits durably but its
envelope never reaches the caller (stdout discarded — the response is
lost):

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db submit --intent "Walkthrough: recovery leg" --idempotency-key walkthrough-recover-1 > /dev/null
$ echo $?
0
```

Recovery runs from the retained `(origin, actor, idempotency_key)` —
the original key, never a fresh one:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db command-result --origin cli --replay-key walkthrough-recover-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "command_result",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": null,
  "execution_id": null,
  "result": {
    "command_id": "cmd-5",
    "committed_at": 4,
    "effect": {
      "execution_id": "e-2",
      "kind": "work_created"
    },
    "status": "accepted",
    "work_id": "w-2",
    "work_version": 1
  },
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 6
}
$ echo $?
0
```

Identical replay of the lost command returns the stored decision —
`cmd-5` — with no second mutation:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db submit --intent "Walkthrough: recovery leg" --idempotency-key walkthrough-recover-1
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": "cmd-5",
    "kind": "submit",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-recover-1"
  },
  "work_id": "w-2",
  "execution_id": "e-2",
  "result": {
    "execution_id": "e-2",
    "kind": "work_created"
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

The recovery leg also exercises the `run` driver rows (outside the
matrix): the loop runs independently of any agent conversation. One
bounded pass emits exactly one envelope on stdout; per-pass progress is
a stderr diagnostic:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db run --once
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": null,
    "kind": "run",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": null
  },
  "work_id": null,
  "execution_id": null,
  "result": {
    "items": [
      {
        "attention_count": 0,
        "current_execution_id": "e-1",
        "evidence": [],
        "outcome": null,
        "phase": "implementing",
        "version": 1,
        "work_id": "w-1"
      },
      {
        "attention_count": 0,
        "current_execution_id": "e-2",
        "evidence": [],
        "outcome": null,
        "phase": "queued",
        "version": 1,
        "work_id": "w-2"
      }
    ],
    "next_cursor": null
  },
  "reason": null,
  "evidence_refs": [],
  "sources": [
    {
      "source": "store",
      "availability": "known",
      "observed_unix_seconds": null
    }
  ],
  "as_of": 16
}
--- stderr ---
taisan: pass 1: 8 phase(s)
$ echo $?
0
```

At this point the store holds the original Work plus the recovered one;
a fresh key for the same payload is a **new** Command, not a recovery:
it queued a second Work (`w-3`). This is exactly why an `unknown` or
lost response must recover through `command-result` or byte-identical
replay only:

```text
$ taisan --db /runs/20261115T100027Z-3632335b/worker-root/workspaces/20261115T100046Z-5de1a607/target/u74-capture/walk/state/store.db submit --intent "Walkthrough: recovery leg" --idempotency-key walkthrough-recover-2
{
  "schema_version": "1",
  "status": "accepted",
  "command": {
    "id": "cmd-6",
    "kind": "submit",
    "actor": "human:actor-18e8319b045d1202-4465",
    "idempotency_key": "walkthrough-recover-2"
  },
  "work_id": "w-3",
  "execution_id": "e-3",
  "result": {
    "execution_id": "e-3",
    "kind": "work_created"
  },
  "reason": null,
  "evidence_refs": []
}
$ echo $?
0
```

Work recorded above remains inspectable after this session ends —
conversation closure is not cancellation, and only an explicit `cancel`
records a stop intent.

## Verification

The walkthrough and the matrix are pinned by named checks in
`crates/workloop-cli/tests/`:

- `u74_matrix.rs::every_applicable_matrix_row_passes_through_the_installed_skill_and_the_direct_cli`
  — every applicable Command/Query row through the installed skill and
  the direct CLI with identical envelope/status per row.
- `u74_matrix.rs::a_clean_skill_install_into_a_fresh_environment_invokes_once_end_to_end`
  — the clean install into a pristine environment and one end-to-end
  invocation.
- `u74_docs.rs::walkthrough_doc_check_every_step_section_carries_verbatim_captured_cli_output`
  — this document: one section per step, each with captured output.
- `u74_docs.rs::evidence_labels_every_matrix_result_fixture_or_live`
  and `u74_docs.rs::evidence_record_cites_the_tested_revision_or_the_explicit_gap`
  — the [evidence record](evidence/u7.4.md).

## Claims boundary

Every result captured above is `fixture`. No row in this walkthrough is
labeled `live`; live U8 evidence, when authorized and run, is recorded
separately under `docs/evidence/u8*.md`. Fixture results support
contract claims only.
