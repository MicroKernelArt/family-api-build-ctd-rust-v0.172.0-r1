#!/usr/bin/env bash
# Verify immutable base-image references remain multi-architecture indexes.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=deploy/build-sidecar-profile-lib.sh
source "$REPO/deploy/build-sidecar-profile-lib.sh"

assert_index_platforms() {
  local image="$1" raw
  raw="$(docker buildx imagetools inspect --raw "$image")"
  python3 - "$image" "$raw" <<'PY'
import json
import sys

image = sys.argv[1]
index = json.loads(sys.argv[2])
if index.get("mediaType") not in {
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
}:
    raise SystemExit(f"FAIL: {image} is not an image index")
platforms = {
    (entry.get("platform") or {}).get("os", "")
    + "/"
    + (entry.get("platform") or {}).get("architecture", "")
    for entry in index.get("manifests", [])
}
required = {"linux/amd64", "linux/arm64"}
missing = required - platforms
if missing:
    raise SystemExit(f"FAIL: {image} missing platforms: {sorted(missing)}")
print(f"PASS: {image} contains linux/amd64 and linux/arm64")
PY
}

assert_index_platforms "$RUST_IMAGE"
assert_index_platforms "$DEBIAN_IMAGE"

dockerfile_ref="$(sed -n 's/^FROM //p' "$REPO/codex-rs/codex-transportd/Dockerfile.runtime" | head -1)"
[ "$dockerfile_ref" = "$DEBIAN_IMAGE" ] || {
  echo "FAIL: Dockerfile.runtime base '$dockerfile_ref' != profile pin '$DEBIAN_IMAGE'"
  exit 1
}

echo "build-sidecar base image index test passed"
