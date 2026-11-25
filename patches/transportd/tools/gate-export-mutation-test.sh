#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
GOLDEN="$REPO/codex-rs/codex-transportd/tests/fixtures/operations_snapshot.golden.json"

python3 - "$GOLDEN" <<'PY'
import json
import sys

document = json.load(open(sys.argv[1], encoding="utf-8"))
operations = document["operations"]
names = [row["operation"] for row in operations]
if document["operation_count"] != len(operations):
    raise SystemExit("FAIL: golden operation_count does not match rows")
if len(names) != len(set(names)):
    raise SystemExit("FAIL: golden contains duplicate operations")
if not {"models", "file_download", "oauth_whoami"}.issubset(names):
    raise SystemExit("FAIL: golden is missing required closed operations")

mutated = dict(document)
mutated["operations"] = operations[:-1]
if mutated["operation_count"] == len(mutated["operations"]):
    raise SystemExit("FAIL: count guard did not detect a missing row")
print("PASS: operations golden structure and missing-row mutation guard")
PY

(cd "$REPO/codex-rs" && just test -p codex-transportd --test operations_snapshot_golden)
