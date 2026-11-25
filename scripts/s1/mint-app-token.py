#!/usr/bin/env python3
"""Mint a GitHub App installation token for the S1 reviewer App.

Owner-operations tooling: it depends on the owner's local App sidecar
record and the private ``shimpeiws/coding-worker-smoke`` installation —
it cannot run for a public reader.

Reads the local App sidecar record
(``~/.config/coding-worker/github-apps/s1-reviewer.json`` by default,
overridable with ``--config <path>``), builds an RS256 App JWT signed
with the recorded private key via ``openssl``, exchanges it for an
installation token, and prints the token to stdout.

This is the resolver behind the ``review`` role's ``command:``
credential slot: the credential boundary runs this argv per
resolution, so every consumer receives a fresh token inside its
one-hour lifetime. Diagnostics go to stderr; only the token reaches
stdout. No key or token material is ever written to disk or logged.
"""

import argparse
import base64
import json
import subprocess
import sys
import time
import urllib.request
import urllib.error
from pathlib import Path

API = "https://api.github.com"
DEFAULT_CONFIG = Path.home() / ".config/coding-worker/github-apps/s1-reviewer.json"


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def die(msg: str) -> "SystemExit":
    print(f"mint-app-token: {msg}", file=sys.stderr)
    return SystemExit(1)


def app_jwt(app_id: str, private_key_path: Path) -> str:
    now = int(time.time())
    header = {"alg": "RS256", "typ": "JWT"}
    payload = {"iat": now - 60, "exp": now + 540, "iss": str(app_id)}
    signing_input = (
        b64url(json.dumps(header, separators=(",", ":")).encode())
        + "."
        + b64url(json.dumps(payload, separators=(",", ":")).encode())
    )
    proc = subprocess.run(
        [
            "openssl", "dgst", "-sha256", "-sign", str(private_key_path),
        ],
        input=signing_input.encode(),
        capture_output=True,
    )
    if proc.returncode != 0:
        raise die(f"openssl signing failed: {proc.stderr.decode(errors='replace').strip()}")
    return signing_input + "." + b64url(proc.stdout)


def mint_token(jwt: str, installation_id: str) -> str:
    req = urllib.request.Request(
        f"{API}/app/installations/{installation_id}/access_tokens",
        method="POST",
        headers={
            "Authorization": f"Bearer {jwt}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        data=b"{}",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:300]
        raise die(f"access_tokens POST failed: HTTP {e.code}: {detail}")
    token = body.get("token")
    if not token:
        raise die("access_tokens response carried no token")
    return token


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    args = parser.parse_args()

    try:
        record = json.loads(args.config.read_text())
    except OSError as e:
        raise die(f"cannot read config {args.config}: {e}")
    except json.JSONDecodeError as e:
        raise die(f"config {args.config} is not valid JSON: {e}")

    app_id = record.get("app_id")
    installation_id = record.get("installation_id")
    key_path = record.get("private_key_path")
    if not all(isinstance(v, (str, int)) and str(v) for v in (app_id, installation_id)):
        raise die("config must carry app_id and installation_id")
    if not isinstance(key_path, str) or not key_path:
        raise die("config must carry private_key_path")
    # The sidecar convention records the PEM relative to the config
    # file's directory; an absolute path is accepted as recorded.
    pem = Path(key_path).expanduser()
    if not pem.is_absolute():
        pem = args.config.parent / pem
    if not pem.is_file():
        raise die(f"private key not found at {pem}")

    token = mint_token(app_jwt(app_id, pem), installation_id)
    print(token)
    return 0


if __name__ == "__main__":
    sys.exit(main())
