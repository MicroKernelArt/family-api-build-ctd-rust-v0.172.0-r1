#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"
BUILD_RECEIPT="${CTD_BUILD_RECEIPT_PATH:-}"
RECEIPT="${CTD_RELEASE_RECEIPT_PATH:-}"
ARTIFACT="${CTD_RELEASE_ARTIFACT_PATH:-}"
BINARY="${CTD_RELEASE_BINARY_PATH:-}"
SBOM="${CTD_RELEASE_SBOM_PATH:-}"
LICENSE_REPORT="${CTD_RELEASE_LICENSE_REPORT_PATH:-}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

for binding in \
  "CTD_BUILD_RECEIPT_PATH:$BUILD_RECEIPT" \
  "CTD_RELEASE_RECEIPT_PATH:$RECEIPT" \
  "CTD_RELEASE_ARTIFACT_PATH:$ARTIFACT" \
  "CTD_RELEASE_BINARY_PATH:$BINARY" \
  "CTD_RELEASE_SBOM_PATH:$SBOM" \
  "CTD_RELEASE_LICENSE_REPORT_PATH:$LICENSE_REPORT"; do
  name="${binding%%:*}"
  path="${binding#*:}"
  [ -n "$path" ] || fail "$name is required"
  ctd_require_external_receipt "$REPO" "$path"
done
[ -f "$BUILD_RECEIPT" ] || fail "build receipt does not exist: $BUILD_RECEIPT"
for output in "$RECEIPT" "$ARTIFACT" "$BINARY" "$SBOM" "$LICENSE_REPORT"; do
  [ ! -e "$output" ] || fail "refusing to overwrite release output: $output"
  mkdir -p "$(dirname "$output")"
done

[ "$(ctd_field "$BUILD_RECEIPT" schema)" = "ctd/evidence-receipt/v2" ] || {
  fail "unsupported build receipt schema"
}
[ "$(ctd_field "$BUILD_RECEIPT" artifact_kind)" = "oci-image" ] || {
  fail "build receipt must describe an oci-image"
}
[ "$(ctd_field "$BUILD_RECEIPT" build_profile)" = "release" ] || {
  fail "build receipt must use the release profile"
}
[ "$(ctd_field "$BUILD_RECEIPT" build_mode)" = "local-cargo" ] || {
  fail "build receipt must use local-cargo mode"
}
[ "$(ctd_field "$BUILD_RECEIPT" platform)" = "linux/amd64" ] || {
  fail "build receipt must target linux/amd64"
}

SOURCE_COMMIT="$(ctd_field "$BUILD_RECEIPT" source_commit)"
SOURCE_TREE="$(ctd_field "$BUILD_RECEIPT" source_tree)"
REVIEW_COMMIT="$(ctd_field "$BUILD_RECEIPT" review_commit)"
REVIEW_TREE="$(ctd_field "$BUILD_RECEIPT" review_tree)"
RELEASE_ID="$(ctd_field "$BUILD_RECEIPT" release_id)"
IMAGE="$(ctd_field "$BUILD_RECEIPT" image)"
[ "$SOURCE_COMMIT" = "$(ctd_field "$MANIFEST" source_commit)" ] || {
  fail "build receipt source_commit mismatch"
}
[ "$SOURCE_TREE" = "$(ctd_field "$MANIFEST" source_tree)" ] || {
  fail "build receipt source_tree mismatch"
}
[ "$RELEASE_ID" = "$(ctd_field "$MANIFEST" release_id)" ] || {
  fail "build receipt release_id mismatch"
}
[ "$(git -C "$REPO" rev-parse "$SOURCE_COMMIT^{tree}")" = "$SOURCE_TREE" ] || {
  fail "build receipt source identity mismatch"
}
[ "$(git -C "$REPO" rev-parse "$REVIEW_COMMIT^{tree}")" = "$REVIEW_TREE" ] || {
  fail "build receipt review identity mismatch"
}

IMAGE_DIGEST="$(docker image inspect "$IMAGE" --format '{{.Id}}')"
case "$IMAGE_DIGEST" in
  sha256:*) ctd_require_hex image_digest "${IMAGE_DIGEST#sha256:}" 64 ;;
  *) fail "Docker image ID must use sha256" ;;
esac

docker save --output "$ARTIFACT" "$IMAGE_DIGEST"
[ -s "$ARTIFACT" ] || fail "docker save produced an empty artifact"

CONTAINER_ID=""
cleanup() {
  if [ -n "$CONTAINER_ID" ]; then
    docker rm -f "$CONTAINER_ID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
CONTAINER_ID="$(docker create "$IMAGE_DIGEST")"
docker cp "$CONTAINER_ID":/app/codex-transportd "$BINARY"
[ -s "$BINARY" ] || fail "image binary is missing or empty"

ARTIFACT_SHA="$(ctd_sha256 "$ARTIFACT")"
BINARY_SHA="$(ctd_sha256 "$BINARY")"
CARGO_LOCK_SHA="$(ctd_field "$MANIFEST" cargo_lock_sha256)"
BAZEL_LOCK_SHA="$(ctd_field "$MANIFEST" module_bazel_lock_sha256)"

python3 - \
  "$SBOM" "$SOURCE_COMMIT" "$SOURCE_TREE" "$RELEASE_ID" \
  "$ARTIFACT_SHA" "$IMAGE_DIGEST" "$BINARY_SHA" \
  "$CARGO_LOCK_SHA" "$BAZEL_LOCK_SHA" <<'PY'
import json
import sys
from pathlib import Path

(
    output,
    source_commit,
    source_tree,
    release_id,
    artifact_sha,
    image_digest,
    binary_sha,
    cargo_lock_sha,
    bazel_lock_sha,
) = sys.argv[1:]
bom = {
    "bomFormat": "CycloneDX",
    "specVersion": "1.5",
    "serialNumber": f"urn:uuid:{source_commit[:8]}-{source_commit[8:12]}-4000-8000-{source_commit[12:24]}",
    "version": 1,
    "metadata": {
        "component": {
            "type": "container",
            "name": "codex-transportd-release-image",
            "version": release_id,
            "hashes": [{"alg": "SHA-256", "content": artifact_sha}],
            "properties": [
                {"name": "ctd:image_digest", "value": image_digest},
                {"name": "ctd:source_commit", "value": source_commit},
                {"name": "ctd:source_tree", "value": source_tree},
            ],
        }
    },
    "components": [
        {
            "type": "application",
            "name": "codex-transportd",
            "version": release_id,
            "hashes": [{"alg": "SHA-256", "content": binary_sha}],
            "licenses": [{"license": {"id": "Apache-2.0"}}],
        },
        {
            "type": "file",
            "name": "codex-rs/Cargo.lock",
            "hashes": [{"alg": "SHA-256", "content": cargo_lock_sha}],
        },
        {
            "type": "file",
            "name": "MODULE.bazel.lock",
            "hashes": [{"alg": "SHA-256", "content": bazel_lock_sha}],
        },
    ],
}
Path(output).write_text(json.dumps(bom, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

python3 - "$LICENSE_REPORT" "$ARTIFACT_SHA" "$BINARY_SHA" <<'PY'
import json
import sys
from pathlib import Path

output, artifact_sha, binary_sha = sys.argv[1:]
report = {
    "schema": "ctd/license-report/v1",
    "artifactDigest": f"sha256:{artifact_sha}",
    "binaryDigest": f"sha256:{binary_sha}",
    "licenses": [{"expression": "Apache-2.0", "scope": "repository-declared"}],
}
Path(output).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

SBOM_SHA="$(ctd_sha256 "$SBOM")"
LICENSE_REPORT_SHA="$(ctd_sha256 "$LICENSE_REPORT")"
cat >"$RECEIPT" <<EOF
schema: ctd/evidence-receipt/v2
artifact_kind: oci-image
artifact_format: docker-archive
artifact_path: $ARTIFACT
artifact_digest: sha256:$ARTIFACT_SHA
image: $IMAGE
image_digest: $IMAGE_DIGEST
binary_path: $BINARY
binary_digest: sha256:$BINARY_SHA
sbom_format: cyclonedx-json
sbom_path: $SBOM
sbom_sha256: $SBOM_SHA
sbom_artifact_digest: sha256:$ARTIFACT_SHA
sbom_binary_digest: sha256:$BINARY_SHA
license_report_path: $LICENSE_REPORT
license_report_sha256: $LICENSE_REPORT_SHA
build_profile: release
platform: linux/amd64
arch: amd64
release_id: $RELEASE_ID
source_commit: $SOURCE_COMMIT
source_tree: $SOURCE_TREE
review_commit: $REVIEW_COMMIT
review_tree: $REVIEW_TREE
build_mode: local-cargo
lock_sha256: $(ctd_field "$BUILD_RECEIPT" lock_sha256)
v8_sha256: $(ctd_field "$BUILD_RECEIPT" v8_sha256)
materialized: $(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF

echo "PASS: materialized release artifact $ARTIFACT"
echo "PASS: bound image $IMAGE_DIGEST binary sha256:$BINARY_SHA"
echo "PASS: wrote digest-bound SBOM and receipt $RECEIPT"
