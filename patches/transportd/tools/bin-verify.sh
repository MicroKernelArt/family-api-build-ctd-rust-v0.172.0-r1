#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
RECEIPT=""
ARTIFACT=""
BINARY=""

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --receipt) RECEIPT="${2:-}"; shift 2 ;;
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --binary) BINARY="${2:-}"; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done
for binding in "receipt:$RECEIPT" "artifact:$ARTIFACT" "binary:$BINARY"; do
  name="${binding%%:*}"
  path="${binding#*:}"
  [ -n "$path" ] || fail "--$name is required"
  ctd_require_external_receipt "$REPO" "$path"
  [ -s "$path" ] || fail "$name is missing or empty: $path"
done

same_path() {
  [ "$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1")" = \
    "$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$2")" ]
}

same_path "$ARTIFACT" "$(ctd_field "$RECEIPT" artifact_path)" || {
  fail "artifact path does not match receipt"
}
same_path "$BINARY" "$(ctd_field "$RECEIPT" binary_path)" || {
  fail "binary path does not match receipt"
}
ARTIFACT_DIGEST="$(ctd_field "$RECEIPT" artifact_digest)"
BINARY_DIGEST="$(ctd_field "$RECEIPT" binary_digest)"
IMAGE_DIGEST="$(ctd_field "$RECEIPT" image_digest)"
[ "$ARTIFACT_DIGEST" = "sha256:$(ctd_sha256 "$ARTIFACT")" ] || {
  fail "release artifact digest mismatch"
}
[ "$BINARY_DIGEST" = "sha256:$(ctd_sha256 "$BINARY")" ] || {
  fail "release binary digest mismatch"
}
case "$IMAGE_DIGEST" in
  sha256:*) ctd_require_hex image_digest "${IMAGE_DIGEST#sha256:}" 64 ;;
  *) fail "image_digest must use sha256" ;;
esac

python3 - "$ARTIFACT" "$IMAGE_DIGEST" <<'PY'
import hashlib
import json
import sys
import tarfile

artifact, image_digest = sys.argv[1:]
expected = image_digest.removeprefix("sha256:")
with tarfile.open(artifact, "r:*") as archive:
    manifest_file = archive.extractfile("manifest.json")
    if manifest_file is None:
        raise SystemExit("FAIL: Docker archive is missing manifest.json")
    manifest = json.load(manifest_file)
    if not isinstance(manifest, list) or len(manifest) != 1:
        raise SystemExit("FAIL: Docker archive must contain exactly one image")
    config_name = manifest[0].get("Config")
    if not isinstance(config_name, str):
        raise SystemExit("FAIL: Docker archive manifest is missing Config")
    config_file = archive.extractfile(config_name)
    if config_file is None:
        raise SystemExit("FAIL: Docker archive image config is missing")
    actual = hashlib.sha256(config_file.read()).hexdigest()
    if actual != expected:
        raise SystemExit("FAIL: Docker archive image ID does not match receipt")
PY

[ "$(docker image inspect "$IMAGE_DIGEST" --format '{{.Id}}')" = "$IMAGE_DIGEST" ] || {
  fail "materialized image ID is not loaded"
}
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-bin-verify.XXXXXX")"
CONTAINER_ID=""
cleanup() {
  if [ -n "$CONTAINER_ID" ]; then
    docker rm -f "$CONTAINER_ID" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
CONTAINER_ID="$(docker create "$IMAGE_DIGEST")"
docker cp "$CONTAINER_ID":/app/codex-transportd "$WORK/codex-transportd"
cmp -s "$BINARY" "$WORK/codex-transportd" || fail "receipt binary differs from image binary"
[ "sha256:$(ctd_sha256 "$WORK/codex-transportd")" = "$BINARY_DIGEST" ] || {
  fail "image binary digest mismatch"
}
file "$WORK/codex-transportd" | grep -q 'ELF' || fail "release binary is not ELF"

echo "PASS: Docker archive image ID $IMAGE_DIGEST"
echo "PASS: image binary $BINARY_DIGEST"
