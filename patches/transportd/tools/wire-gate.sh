#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"
WIRE_DOC="${WIRE_DOC:-$REPO/WIRE_COMPATIBILITY.md}"
RELEASE_MODE=0
[ "$#" -le 1 ] || { echo "FAIL: wire-gate accepts only --release" >&2; exit 1; }
if [ "$#" -eq 1 ]; then
  [ "$1" = "--release" ] || { echo "FAIL: unknown argument: $1" >&2; exit 1; }
  RELEASE_MODE=1
fi

[ "$(ctd_field "$WIRE_DOC" wire_source_commit)" = "$(ctd_field "$MANIFEST" source_commit)" ] || {
  echo "FAIL: wire source commit mismatch" >&2; exit 1; }
[ "$(ctd_field "$WIRE_DOC" wire_source_tree)" = "$(ctd_field "$MANIFEST" source_tree)" ] || {
  echo "FAIL: wire source tree mismatch" >&2; exit 1; }
STATUS="$(ctd_field "$WIRE_DOC" wire_status)"
case "$STATUS" in
  pass|unverified) ;;
  *) echo "FAIL: invalid wire status: $STATUS" >&2; exit 1 ;;
esac
if [ "$RELEASE_MODE" -eq 1 ] && [ "$STATUS" != "pass" ]; then
  echo "FAIL: wire evidence is not release-approved" >&2
  exit 1
fi
echo "PASS: wire metadata status=$STATUS"
