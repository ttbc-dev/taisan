# AGENTS.md

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
