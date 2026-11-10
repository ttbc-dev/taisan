#!/usr/bin/env bash
# S1 scenario worker (U8.2): a real worker process the supervisor spawns
# as `worker_argv` with cwd = the Work's workspace. It performs the
# seeded issue's deterministic change inside a real clone of the
# isolated repository — the workspace directory IS the repository —
# commits on `work/<work_id>`, runs the recorded verification command,
# and writes the required `artifact.json` at the workspace root.
#
# The shim is deliberately NOT a coding-agent runtime (the reference
# Worker shim is U4.2, unimplemented): it is a deterministic worker
# whose rounds are keyed to workspace state — the first Invocation
# produces the requested change (leaving the engineered review-visible
# gap), a later Fix Invocation produces the review fix. It holds no
# repository-host credential and never performs a host write:
# publication is the adapter side's job.
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

if ! git rev-parse --verify -q "$branch" >/dev/null; then
    git checkout -q -b "$branch" origin/main
else
    git checkout -q "$branch"
    git merge -q --ff-only "origin/main" 2>/dev/null || true
fi

if git log --format=%s -20 "$branch" | grep "Add greet helper" >/dev/null; then
    round=2
    python3 - <<'PY'
import pathlib

p = pathlib.Path("sample.py")
src = p.read_text()
old = '''def greet(name):
    if not name or not name.strip():
        return "Hello!"
    return f"Hello, {name.strip()}!"
'''
new = '''def greet(name):
    """Return a greeting for NAME, defaulting to a bare 'Hello!'.

    Empty, whitespace-only, and None callers all produce 'Hello!'.
    """
    if name is None:
        return "Hello!"
    if not name or not name.strip():
        return "Hello!"
    return f"Hello, {name.strip()}!"
'''
assert old in src, "round-1 greet body not found"
p.write_text(src.replace(old, new))
PY
    git add sample.py
    git commit -q -m "Guard greet input and document it"
else
    round=1
    cat >> sample.py <<'EOF'


def greet(name):
    if not name or not name.strip():
        return "Hello!"
    return f"Hello, {name.strip()}!"
EOF
    git add sample.py
    git commit -q -m "Add greet helper"
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
    "worker": "s1-shim-v1",
    "round": round_,
    "commit": commit,
    "verification_argv": ["python3", "-m", "unittest", "discover", "-s", "tests", "-v"],
    "verification_exit": status,
}
with open(f"{ws}/artifact.json", "w") as f:
    json.dump(artifact, f, indent=2)
    f.write("\n")
PY

echo "{\"evidence\": \"s1-shim\", \"round\": $round, \"commit\": \"$commit\", \"verification_exit\": $verify_status}"
exit "$verify_status"
