#!/usr/bin/env bash
# Repository setup check (remote worker runs).
#
# The remote verify profile requires `cargo build/test/fmt/clippy`, but the
# worker image carries no Rust toolchain. This hook installs a minimal rustup
# toolchain into $HOME (the remote job resolves $HOME/.cargo/bin for cargo).
# It runs in the job's check phase inside this checkout and must stay
# non-interactive, idempotent and bounded.
set -euo pipefail

# Workspace materialization fetches from a local source with
# `git fetch <path>`, which spawns `git-upload-pack` inside that source.
# A repo-local `uploadpack.packObjectsHook` is ignored only from git
# 2.30.2 onward (protected-configuration scoping); older git would honor
# it. Fail closed with the same named diagnostic the preflight uses.
MIN_GIT_VERSION="2.30.2"
if ! command -v git >/dev/null 2>&1; then
  echo "git-version-unsupported: git is not installed (requires >= ${MIN_GIT_VERSION})" >&2
  exit 1
fi
git_version="$(git --version | awk '{print $3}')"
first_version="$(printf '%s\n%s\n' "$MIN_GIT_VERSION" "$git_version" | sort -V | head -n1)"
if [ "$first_version" != "$MIN_GIT_VERSION" ]; then
  echo "git-version-unsupported: found git ${git_version:-unknown}, requires >= ${MIN_GIT_VERSION}" >&2
  exit 1
fi

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
