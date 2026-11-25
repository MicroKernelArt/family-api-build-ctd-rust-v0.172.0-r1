#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"
SBOM_DOC="${SBOM_DOC:-$REPO/SBOM.md}"
RECEIPT=""
ARTIFACT=""
BINARY=""
SBOM=""
LICENSE_REPORT=""
RELEASE_MODE=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --receipt) RECEIPT="${2:-}"; shift 2 ;;
    --artifact) ARTIFACT="${2:-}"; shift 2 ;;
    --binary) BINARY="${2:-}"; shift 2 ;;
    --sbom) SBOM="${2:-}"; shift 2 ;;
    --license-report) LICENSE_REPORT="${2:-}"; shift 2 ;;
    --release) RELEASE_MODE=1; shift ;;
    *) echo "FAIL: unknown argument: $1" >&2; exit 1 ;;
  esac
done

[ "$(ctd_field "$SBOM_DOC" sbom_source_commit)" = "$(ctd_field "$MANIFEST" source_commit)" ] || {
  echo "FAIL: SBOM source commit mismatch" >&2; exit 1; }
[ "$(ctd_field "$SBOM_DOC" sbom_source_tree)" = "$(ctd_field "$MANIFEST" source_tree)" ] || {
  echo "FAIL: SBOM source tree mismatch" >&2; exit 1; }
[ "$(ctd_field "$SBOM_DOC" sbom_cargo_lock_sha256)" = "$(ctd_field "$MANIFEST" cargo_lock_sha256)" ] || {
  echo "FAIL: SBOM Cargo.lock mismatch" >&2; exit 1; }
[ "$(ctd_field "$SBOM_DOC" sbom_module_bazel_lock_sha256)" = "$(ctd_field "$MANIFEST" module_bazel_lock_sha256)" ] || {
  echo "FAIL: SBOM MODULE.bazel.lock mismatch" >&2; exit 1; }
[ "$(ctd_field "$SBOM_DOC" sbom_status)" = "generated-from-locks" ] || {
  echo "FAIL: SBOM status is not generated-from-locks" >&2; exit 1; }
[ "$(ctd_field "$SBOM_DOC" sbom_artifact_status)" = "external-receipt-required" ] || {
  echo "FAIL: SBOM artifact policy does not require an external receipt" >&2; exit 1; }

if [ "$RELEASE_MODE" -eq 1 ]; then
  [ -n "$RECEIPT" ] || { echo "FAIL: release SBOM check requires --receipt" >&2; exit 1; }
  ctd_require_external_receipt "$REPO" "$RECEIPT"
  [ -s "$RECEIPT" ] || { echo "FAIL: release SBOM receipt is missing or empty" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" artifact_kind)" = "oci-image" ] || {
    echo "FAIL: release SBOM receipt must describe an oci-image" >&2; exit 1; }
  for binding in \
    "artifact:$ARTIFACT" \
    "binary:$BINARY" \
    "sbom:$SBOM" \
    "license-report:$LICENSE_REPORT"; do
    name="${binding%%:*}"
    path="${binding#*:}"
    [ -n "$path" ] || { echo "FAIL: release SBOM check requires --$name" >&2; exit 1; }
    ctd_require_external_receipt "$REPO" "$path"
    [ -s "$path" ] || { echo "FAIL: release $name is missing or empty: $path" >&2; exit 1; }
  done

  same_path() {
    [ "$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1")" = \
      "$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$2")" ]
  }
  same_path "$ARTIFACT" "$(ctd_field "$RECEIPT" artifact_path)" || {
    echo "FAIL: SBOM artifact path does not match receipt" >&2; exit 1; }
  same_path "$BINARY" "$(ctd_field "$RECEIPT" binary_path)" || {
    echo "FAIL: SBOM binary path does not match receipt" >&2; exit 1; }
  same_path "$SBOM" "$(ctd_field "$RECEIPT" sbom_path)" || {
    echo "FAIL: SBOM path does not match receipt" >&2; exit 1; }
  same_path "$LICENSE_REPORT" "$(ctd_field "$RECEIPT" license_report_path)" || {
    echo "FAIL: license report path does not match receipt" >&2; exit 1; }

  ARTIFACT_DIGEST="sha256:$(ctd_sha256 "$ARTIFACT")"
  BINARY_DIGEST="sha256:$(ctd_sha256 "$BINARY")"
  [ "$(ctd_field "$RECEIPT" artifact_digest)" = "$ARTIFACT_DIGEST" ] || {
    echo "FAIL: SBOM artifact digest mismatch" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" binary_digest)" = "$BINARY_DIGEST" ] || {
    echo "FAIL: SBOM binary digest mismatch" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" sbom_artifact_digest)" = "$ARTIFACT_DIGEST" ] || {
    echo "FAIL: SBOM is not bound to the release artifact" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" sbom_binary_digest)" = "$BINARY_DIGEST" ] || {
    echo "FAIL: SBOM is not bound to the image binary" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" sbom_format)" = "cyclonedx-json" ] || {
    echo "FAIL: unsupported release SBOM format" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" sbom_sha256)" = "$(ctd_sha256 "$SBOM")" ] || {
    echo "FAIL: release SBOM hash mismatch" >&2; exit 1; }
  [ "$(ctd_field "$RECEIPT" license_report_sha256)" = "$(ctd_sha256 "$LICENSE_REPORT")" ] || {
    echo "FAIL: license report hash mismatch" >&2; exit 1; }

  python3 - \
    "$SBOM" "$LICENSE_REPORT" "$ARTIFACT_DIGEST" "$BINARY_DIGEST" \
    "$(ctd_field "$MANIFEST" source_commit)" \
    "$(ctd_field "$MANIFEST" source_tree)" <<'PY'
import json
import sys

sbom_path, license_path, artifact_digest, binary_digest, source_commit, source_tree = sys.argv[1:]
with open(sbom_path, encoding="utf-8") as handle:
    sbom = json.load(handle)
if sbom.get("bomFormat") != "CycloneDX" or sbom.get("specVersion") not in {"1.5", "1.6"}:
    raise SystemExit("FAIL: release SBOM is not supported CycloneDX JSON")
components = sbom.get("components")
if not isinstance(components, list) or not components:
    raise SystemExit("FAIL: release SBOM has no components")
metadata_component = sbom.get("metadata", {}).get("component", {})
metadata_hashes = metadata_component.get("hashes", [])
artifact_hex = artifact_digest.removeprefix("sha256:")
if not any(item.get("alg") == "SHA-256" and item.get("content") == artifact_hex for item in metadata_hashes):
    raise SystemExit("FAIL: release SBOM metadata does not bind the artifact digest")
properties = {
    item.get("name"): item.get("value")
    for item in metadata_component.get("properties", [])
    if isinstance(item, dict)
}
if properties.get("ctd:source_commit") != source_commit or properties.get("ctd:source_tree") != source_tree:
    raise SystemExit("FAIL: release SBOM source identity mismatch")
binary_hex = binary_digest.removeprefix("sha256:")
binary_components = [item for item in components if item.get("name") == "codex-transportd"]
if not any(
    hash_item.get("alg") == "SHA-256" and hash_item.get("content") == binary_hex
    for component in binary_components
    for hash_item in component.get("hashes", [])
):
    raise SystemExit("FAIL: release SBOM does not bind the image binary")
with open(license_path, encoding="utf-8") as handle:
    license_report = json.load(handle)
if license_report.get("schema") != "ctd/license-report/v1":
    raise SystemExit("FAIL: unsupported license report schema")
if license_report.get("artifactDigest") != artifact_digest:
    raise SystemExit("FAIL: license report artifact digest mismatch")
if license_report.get("binaryDigest") != binary_digest:
    raise SystemExit("FAIL: license report binary digest mismatch")
licenses = license_report.get("licenses")
if not isinstance(licenses, list) or not licenses:
    raise SystemExit("FAIL: license report has no licenses")
PY
fi
echo "PASS: SBOM source and dependency-lock identities"
if [ "$RELEASE_MODE" -eq 1 ]; then
  echo "PASS: digest-bound release SBOM and license report"
fi
