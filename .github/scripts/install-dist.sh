#!/bin/sh
# Fetch, verify, and run the pinned cargo-dist shell installer.
#
# release.yml uses this instead of dist's emitted
# `curl ... cargo-dist-installer.sh | sh` so a compromised or corrupted
# installer fails closed instead of executing in the release pipeline.
# cargo-dist publishes no checksum for the installer script itself — its
# sha256.sum covers only the archives — so the expected digest is
# committed here. The verified script in turn embeds the sha256 of every
# cargo-dist tarball it downloads, so the whole fetch chain is bound to
# this one digest.
#
# To bump: set DIST_VERSION to the new tag (keep it in lockstep with
# [workspace.metadata.dist].cargo-dist-version), then run
#   curl --proto '=https' --tlsv1.2 -LsSf \
#     "https://github.com/axodotdev/cargo-dist/releases/download/v<DIST_VERSION>/cargo-dist-installer.sh" \
#     | shasum -a 256
# and replace INSTALLER_SHA256.
set -eu

DIST_VERSION="0.34.0"
INSTALLER_SHA256="abb014a8486f08a3c2d9dd8bbf2c646c62fab61b1f1c9ff5bf2811ab305d17b1"

installer="$(mktemp)"
trap 'rm -f "$installer"' EXIT

curl --proto '=https' --tlsv1.2 -LsSf \
    "https://github.com/axodotdev/cargo-dist/releases/download/v${DIST_VERSION}/cargo-dist-installer.sh" \
    -o "$installer"

# sha256sum on Linux runners, shasum on macOS runners.
if command -v sha256sum > /dev/null 2>&1; then
    actual="$(sha256sum "$installer" | awk '{print $1}')"
else
    actual="$(shasum -a 256 "$installer" | awk '{print $1}')"
fi

if [ "$actual" != "$INSTALLER_SHA256" ]; then
    echo "cargo-dist-installer.sh v${DIST_VERSION} sha256 mismatch" >&2
    echo "  expected: $INSTALLER_SHA256" >&2
    echo "  actual:   $actual" >&2
    exit 1
fi

sh "$installer"
