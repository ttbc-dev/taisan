# External Selector protocol document verification

Issue: #200. This record covers documentation and static JSON examples only. No external Selector process, source host, Worker invocation, publication, or owner-run live scenario was exercised.

## Tested material and configuration

The tested runtime baseline is `b2b03e872f8be51e06e242228355f29c013c5c24`. The documentation changes are on `issue-200-stateless-selector-protocol`; the commit containing this record identifies the reviewed document revision. The contract is [protocol version 1](../selector-protocol.md), with its [structural schema](../selector-protocol.schema.json).

The static example configuration is adapter `public-issue-selector`, selector version `1`, policy revision `7`, capability `grouped_sources_v1`, and illustrative argv `["public-issue-selector", "select"]`. The argv was not invoked. Request construction time is fixed at `1794787200000` Unix milliseconds for example interpretation, with deadline `1794787230000`, two maximum candidates, repository `public-host` / `repo-17`, and excluded issue `issue-9`. These are synthetic scoped identities, not configured live endpoints. Task defaults and permissions are retained in [the request](../examples/selector-protocol/request.json). The fixed deadline is sample data, not a command to run now.

## Static example results

Python with `jsonschema` validates the schema using `Draft202012Validator.check_schema` and validates all four example documents. These are document checks, not the future launcher's semantic validator.

| Check | Actual result |
| --- | --- |
| Request structural schema | Passed |
| `grouped-success` structural schema | Passed, one candidate containing two source bindings |
| `empty-scan` structural schema | Passed, zero candidates |
| `source-failure` structural schema | Passed, `source_unavailable` failure |
| Grouped example semantic digest recomputation | Matched `b636ae575bbef3269289f01a74a10fe1b10502572e1fe40bf6426c2798f57447` |
| `unsupported-wire` mutation to version `2` | Schema rejected |
| `bad-second-candidate` mutation appending an empty object | Schema rejected |
| `invalid-task` mutation removing `intent` | Schema rejected |
| Unknown top-level field mutation | Schema rejected |

The digest check reconstructs the flattened `proposal-v1` semantic payload, sorts object keys, serializes compact UTF-8 JSON, and hashes the `proposal-v1:` prefix plus those bytes. This confirms the ASCII example's digest. It does not prove cross-language canonicalization for every possible string.

The protocol's other named scenarios remain required tests for dependent implementation. In particular, no whole-response admission atomicity, ownership race, process timeout, byte-limit enforcement, secret isolation, or grouped execution behavior is claimed from these static checks. Existing Rust fixture tests cover the current snapshot Selector and submit path, not a future external adapter.

To repeat the structural example checks from the repository root:

```bash
uv run --with jsonschema python - <<'PY'
import json
from pathlib import Path
from jsonschema import Draft202012Validator
schema = json.loads(Path("docs/selector-protocol.schema.json").read_text())
Draft202012Validator.check_schema(schema)
validator = Draft202012Validator(schema)
for path in sorted(Path("docs/examples/selector-protocol").glob("*.json")):
	validator.validate(json.loads(path.read_text()))
	print("PASS", path)
PY
```

## Repository verification

| Command or check | Actual result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo build --workspace` | Passed |
| `cargo test --workspace` | Failed during test compilation. Existing `ProcessState` initializers omit `exit_code` in `crates/workloop-github/tests/github_adapter.rs` at lines 862 and 1388 |
| `cargo clippy --workspace --all-targets -- -D warnings` | Failed with the same two existing missing-`exit_code` test initializers |
| `cargo test -p workloop-selector` | Passed, 6 admission contract tests and 14 Selector contract tests |
| Local links in the protocol, boundary, and evidence documents | Passed |
| `git diff --check` | Passed |

The workspace-test failures occur in unchanged Rust files at the tested baseline. The original #200 commit changes documentation and JSON examples only. Live evidence is absent and is not inferred from fixture results.

## CI fixture correction for #221

The first PR check on document revision `7b3c24c` reproduced the two missing `exit_code` initializers. After those compiled, three completion tests exposed another outdated fixture: `waiting_with_applied_publish` omitted the invariant-check Invocation and its own successful exit observation. An assessment alone does not satisfy that gate.

The #221 correction adds the completed invariant-check Invocation and `Exited` observation with `Some(0)` to that shared fixture. The cancellation and pause observations use `exit_code: None`, because they prove stopping without claiming a successful check. Production behavior and the external Selector protocol are unchanged.

After this correction, the following commands passed locally on the same runtime baseline:

- `cargo fmt --check`
- `cargo build --locked --workspace`
- `cargo test --locked -p workloop-github --test github_adapter`, all 21 tests
- `cargo test --locked --workspace`
- `cargo clippy --locked --workspace --all-targets -- -D warnings`

These results supersede the initial repository-check failures above. They remain fixture evidence, with no live execution.
