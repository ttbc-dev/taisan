#!/usr/bin/env bash
# S1 scenario worker (U8.3): the non-converging variant of the S1
# scenario worker. Same real spawned-worker contract as worker-shim.sh
# — the supervisor spawns it as `worker_argv` with cwd = the Work's
# workspace, it clones the isolated repository, commits on
# `work/<work_id>`, runs the recorded verification argv, and writes
# `artifact.json` — but its produced change can never satisfy the
# review contract: every round emits a greet() variant that passes the
# repository verification while still carrying the seeded review-visible
# gaps (no docstring, no explicit None literal). Review therefore flags
# every published head; the run can only settle under a policy bound,
# which is exactly the bounded/non-converging condition U8.3 evidences.
#
# Rounds are keyed to committed history on the work branch: the first
# Invocation (Coding) produces round 1, each later Fix Invocation
# produces the next numbered variant. The variant index is embedded in
# the emitted code so every round commits distinct content. The
# pre-publication invariant check re-runs this same argv against a
# snapshot copy under a `check-*` name; like worker-shim.sh it adopts
# the work branch HEAD already names and verifies read-only.
set -euo pipefail

ws="$PWD"
work_id="$(basename "$ws")"
work_id="${work_id#ws-}"
branch="work/$work_id"
remote="https://github.com/shimpeiws/coding-worker-smoke.git"
verify_log="$ws/verification.log"

if [ ! -d "$ws/.git" ]; then
    git clone --quiet "$remote" "$ws"
fi
cd "$ws"

git config user.name "s1-worker"
git config user.email "s1-worker@localhost"

verify_only=0
if ! git rev-parse --verify -q "$branch" >/dev/null; then
    # The derived work branch is absent. In the check snapshot the
    # workspace copy still has HEAD on the producing work branch —
    # adopt it and verify that exact state read-only. A fresh clone
    # sits on the default branch instead, so this never misfires on
    # a real first Invocation.
    current="$(git branch --show-current)"
    case "$current" in
        work/*)
            branch="$current"
            verify_only=1
            ;;
        *)
            git checkout -q -b "$branch" origin/main
            ;;
    esac
else
    git checkout -q "$branch"
    git merge -q --ff-only "origin/main" 2>/dev/null || true
fi

if [ "$verify_only" = 1 ]; then
    round=0
else
    round=$(( $(git rev-list --count origin/main..HEAD) + 1 ))

    python3 - "$round" <<'PY'
import pathlib, sys

n = int(sys.argv[1])
p = pathlib.Path("sample.py")
head = p.read_text().split("def greet(", 1)[0]
if n % 2 == 1:
    body = f'''def greet(name):
    # s1-nc attempt {n}: trim-and-format implementation.
    if not name or not name.strip():
        return "Hello!"
    return f"Hello, {{name.strip()}}!"
'''
else:
    body = f'''def greet(name):
    # s1-nc attempt {n}: normalize-then-concat implementation.
    normalized = name.strip() if isinstance(name, str) else ""
    if not normalized:
        return "Hello!"
    return "Hello, " + normalized + "!"
'''
p.write_text(head.rstrip() + "\n\n\n" + body)
PY

    git add sample.py
    git commit -q -m "Add greet helper (s1-nc attempt $round)"
fi

set +e
python3 -m unittest discover -s tests -v >"$verify_log" 2>&1
verify_status=$?
set -e

commit="$(git rev-parse "$branch")"
python3 - "$ws" "$commit" "$round" "$verify_status" <<'PY'
import json, sys
ws, commit, round_, status = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
artifact = {
    "worker": "s1-shim-nc-v1",
    "round": round_,
    "commit": commit,
    "verification_argv": ["python3", "-m", "unittest", "discover", "-s", "tests", "-v"],
    "verification_exit": status,
}
with open(f"{ws}/artifact.json", "w") as f:
    json.dump(artifact, f, indent=2)
    f.write("\n")
PY

echo "{\"evidence\": \"s1-shim-nc\", \"round\": $round, \"commit\": \"$commit\", \"verification_exit\": $verify_status}"
exit "$verify_status"
