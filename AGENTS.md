# AGENTS.md

## Source references

- Do not name or directly reference the private source repositories this
  project was distilled from: no repository names, file paths, or symbol
  names in docs or code. Describe them by role instead (e.g. "the ops
  repo", "the worker repo", "the selector repo", "the infra repo" — see
  `docs/source-workflow-audit.md` for the role legend). Issue numbers and
  abbreviated revision SHAs may be cited as owner-verifiable evidence
  locators.

## Pull requests

- PR titles and descriptions are written in English.

## Verification

```bash
cargo fmt --check
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` contract tests spawn `target/debug/workloop-fixture-worker`, so
run `cargo build --workspace` first — `cargo test` alone does not produce the
fixture-worker binary.
