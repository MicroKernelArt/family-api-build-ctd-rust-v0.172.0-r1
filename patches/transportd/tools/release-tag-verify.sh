#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"
RECEIPT="${CTD_RELEASE_RECEIPT_PATH:-}"
ARTIFACT="${CTD_RELEASE_ARTIFACT_PATH:-}"
BINARY="${CTD_RELEASE_BINARY_PATH:-}"
SBOM="${CTD_RELEASE_SBOM_PATH:-}"
LICENSE_REPORT="${CTD_RELEASE_LICENSE_REPORT_PATH:-}"
[ -n "$RECEIPT" ] || { echo "FAIL: CTD_RELEASE_RECEIPT_PATH is required" >&2; exit 1; }
ctd_require_external_receipt "$REPO" "$RECEIPT"
for binding in \
  "CTD_RELEASE_ARTIFACT_PATH:$ARTIFACT" \
  "CTD_RELEASE_BINARY_PATH:$BINARY" \
  "CTD_RELEASE_SBOM_PATH:$SBOM" \
  "CTD_RELEASE_LICENSE_REPORT_PATH:$LICENSE_REPORT"; do
  name="${binding%%:*}"
  path="${binding#*:}"
  [ -n "$path" ] || { echo "FAIL: $name is required" >&2; exit 1; }
  ctd_require_external_receipt "$REPO" "$path"
done

TAG="$(ctd_field "$MANIFEST" release_tag)"
CURRENT_TAG="$(git -C "$REPO" describe --tags --exact-match HEAD 2>/dev/null || true)"
[ "$CURRENT_TAG" = "$TAG" ] || { echo "FAIL: HEAD is not release tag $TAG" >&2; exit 1; }

bash "$REPO/patches/transportd/tools/release-evidence-gate.sh" \
  --receipt "$RECEIPT" --artifact "$ARTIFACT" --release
bash "$REPO/patches/transportd/tools/bin-verify.sh" \
  --receipt "$RECEIPT" --artifact "$ARTIFACT" --binary "$BINARY"
bash "$REPO/patches/transportd/tools/wire-gate.sh" --release
bash "$REPO/patches/transportd/tools/sbom-gate.sh" \
  --receipt "$RECEIPT" --artifact "$ARTIFACT" --binary "$BINARY" \
  --sbom "$SBOM" --license-report "$LICENSE_REPORT" --release
echo "PASS: release tag, source, artifact, wire, and SBOM identities"
