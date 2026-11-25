#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
GATE="$REPO/patches/transportd/tools/export-gate.sh"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-export-mutation.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
cp -R "$REPO/patches/transportd" "$WORK/transportd"

PATCHES_DIR="$WORK/transportd" "$GATE" >/dev/null
python3 - "$WORK/transportd/series.yaml" <<'PY'
import re
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
text = re.sub(r"^source_tree: [0-9a-f]{40}$", "source_tree: " + "0" * 40, text, count=1, flags=re.M)
open(path, "w", encoding="utf-8").write(text)
PY
if PATCHES_DIR="$WORK/transportd" "$GATE" >"$WORK/gate.log" 2>&1; then
  echo "FAIL: export gate accepted a forged source tree" >&2
  exit 1
fi
grep -qF "source commit/tree mismatch" "$WORK/gate.log" || {
  echo "FAIL: forged-tree failure reason was not deterministic" >&2
  exit 1
}
echo "PASS: export gate rejects source-tree mutation"

REPLAY_MANIFEST="$WORK/replay-manifest.yaml"
cp "$REPO/replay/manifest.yaml" "$REPLAY_MANIFEST"
BASE_COMMIT="$(sed -n 's/^base_commit: //p' "$REPLAY_MANIFEST" | head -1)"
BASE_TREE="$(git -C "$REPO" rev-parse "$BASE_COMMIT^{tree}")"
python3 - "$REPLAY_MANIFEST" "$BASE_COMMIT" "$BASE_TREE" <<'PY'
import re
import sys

path, base_commit, base_tree = sys.argv[1:]
text = open(path, encoding="utf-8").read()
text = re.sub(
    r"^release_commit: [0-9a-f]{40}$",
    f"release_commit: {base_commit}",
    text,
    count=1,
    flags=re.M,
)
text = re.sub(
    r"^expected_release_tree: [0-9a-f]{40}$",
    f"expected_release_tree: {base_tree}",
    text,
    count=1,
    flags=re.M,
)
open(path, "w", encoding="utf-8").write(text)
PY
if REPLAY_MANIFEST="$REPLAY_MANIFEST" bash "$REPO/replay/gate.sh" >"$WORK/replay-gate.log" 2>&1; then
  echo "FAIL: replay gate accepted a release commit behind source_commit" >&2
  exit 1
fi
grep -qF "source_commit is not an ancestor of release_commit" "$WORK/replay-gate.log" || {
  echo "FAIL: invalid release ancestry failure reason was not deterministic" >&2
  exit 1
}
echo "PASS: replay gate rejects invalid release ancestry"
