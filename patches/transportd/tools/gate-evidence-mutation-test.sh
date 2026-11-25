#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
WRITE="$REPO/patches/transportd/tools/write-source-receipt.sh"
GATE="$REPO/patches/transportd/tools/release-evidence-gate.sh"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-evidence-mutation.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
RECEIPT="$WORK/source-receipt.txt"

"$WRITE" "$RECEIPT" >/dev/null
"$GATE" --receipt "$RECEIPT" >/dev/null
python3 - "$RECEIPT" <<'PY'
import re
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
text = re.sub(r"^source_tree: [0-9a-f]{40}$", "source_tree: " + "f" * 40, text, count=1, flags=re.M)
open(path, "w", encoding="utf-8").write(text)
PY
if "$GATE" --receipt "$RECEIPT" >"$WORK/gate.log" 2>&1; then
  echo "FAIL: evidence gate accepted a forged source tree" >&2
  exit 1
fi
grep -qF "receipt source_tree mismatch" "$WORK/gate.log" || {
  echo "FAIL: forged receipt failure reason was not deterministic" >&2
  exit 1
}
echo "PASS: evidence gate rejects source-tree mutation"
