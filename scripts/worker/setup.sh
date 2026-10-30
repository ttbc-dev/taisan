#!/usr/bin/env bash
# Repository setup check (coding-worker remote runs).
#
# The remote verify profile requires `cargo build/test/fmt/clippy`, but the
# worker image carries no Rust toolchain. This hook installs a minimal rustup
# toolchain into $HOME (propose.sh resolves $HOME/.cargo/bin since #112). It
# runs in the job's check phase inside this checkout and must stay
# non-interactive, idempotent and bounded.
set -euo pipefail

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

need_install=0
command -v cargo >/dev/null 2>&1 || need_install=1
command -v rustfmt >/dev/null 2>&1 || need_install=1
command -v cargo-clippy >/dev/null 2>&1 || need_install=1

if [ "$need_install" -eq 0 ]; then
  cargo --version
  exit 0
fi

# Minimal profile keeps the download bounded; edition 2024 requires a recent
# stable. rustfmt/clippy are added because the verify profile runs them.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain stable --profile minimal \
        --component rustfmt --component clippy

cargo --version
rustfmt --version
cargo clippy --version
