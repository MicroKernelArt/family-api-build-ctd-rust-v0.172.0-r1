#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$REPO/patches/transportd/tools/identity-lib.sh"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-wire-sbom-mutation.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

bash "$REPO/patches/transportd/tools/wire-gate.sh" >/dev/null
bash "$REPO/patches/transportd/tools/sbom-gate.sh" >/dev/null

cp "$REPO/WIRE_COMPATIBILITY.md" "$WORK/WIRE_COMPATIBILITY.md"
sed 's/^wire_source_commit: .*/wire_source_commit: 0000000000000000000000000000000000000000/' \
  "$WORK/WIRE_COMPATIBILITY.md" >"$WORK/WIRE_COMPATIBILITY.mutated"
if WIRE_DOC="$WORK/WIRE_COMPATIBILITY.mutated" bash "$REPO/patches/transportd/tools/wire-gate.sh" >/dev/null 2>&1; then
  echo "FAIL: wire gate accepted a forged source commit" >&2
  exit 1
fi

cp "$REPO/SBOM.md" "$WORK/SBOM.md"
sed 's/^sbom_cargo_lock_sha256: .*/sbom_cargo_lock_sha256: 0000000000000000000000000000000000000000000000000000000000000000/' \
  "$WORK/SBOM.md" >"$WORK/SBOM.mutated"
if SBOM_DOC="$WORK/SBOM.mutated" bash "$REPO/patches/transportd/tools/sbom-gate.sh" >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted a forged Cargo.lock digest" >&2
  exit 1
fi

ARTIFACT="$WORK/release-image.tar"
BINARY="$WORK/codex-transportd"
SBOM="$WORK/release.cdx.json"
LICENSE_REPORT="$WORK/license-report.json"
RECEIPT="$WORK/release-receipt.txt"
SBOM_POLICY="$WORK/SBOM.policy.md"
printf 'docker archive fixture\n' >"$ARTIFACT"
printf 'ELF binary fixture\n' >"$BINARY"
ARTIFACT_SHA="$(ctd_sha256 "$ARTIFACT")"
BINARY_SHA="$(ctd_sha256 "$BINARY")"
SOURCE_COMMIT="$(ctd_field "$REPO/patches/transportd/series.yaml" source_commit)"
SOURCE_TREE="$(ctd_field "$REPO/patches/transportd/series.yaml" source_tree)"
sed 's/^sbom_artifact_status: .*/sbom_artifact_status: external-receipt-required/' \
  "$REPO/SBOM.md" >"$SBOM_POLICY"

python3 - "$SBOM" "$LICENSE_REPORT" "$ARTIFACT_SHA" "$BINARY_SHA" \
  "$SOURCE_COMMIT" "$SOURCE_TREE" <<'PY'
import json
import sys
from pathlib import Path

sbom_path, license_path, artifact_sha, binary_sha, source_commit, source_tree = sys.argv[1:]
sbom = {
    "bomFormat": "CycloneDX",
    "specVersion": "1.5",
    "version": 1,
    "metadata": {
        "component": {
            "type": "container",
            "name": "fixture",
            "hashes": [{"alg": "SHA-256", "content": artifact_sha}],
            "properties": [
                {"name": "ctd:source_commit", "value": source_commit},
                {"name": "ctd:source_tree", "value": source_tree},
            ],
        }
    },
    "components": [
        {
            "type": "application",
            "name": "codex-transportd",
            "hashes": [{"alg": "SHA-256", "content": binary_sha}],
        }
    ],
}
license_report = {
    "schema": "ctd/license-report/v1",
    "artifactDigest": f"sha256:{artifact_sha}",
    "binaryDigest": f"sha256:{binary_sha}",
    "licenses": [{"expression": "Apache-2.0"}],
}
Path(sbom_path).write_text(json.dumps(sbom) + "\n", encoding="utf-8")
Path(license_path).write_text(json.dumps(license_report) + "\n", encoding="utf-8")
PY

write_receipt() {
  local output="$1"
  local sbom_path="$2"
  local sbom_artifact_digest="$3"
  cat >"$output" <<EOF
schema: ctd/evidence-receipt/v2
artifact_kind: oci-image
artifact_path: $ARTIFACT
artifact_digest: sha256:$ARTIFACT_SHA
binary_path: $BINARY
binary_digest: sha256:$BINARY_SHA
sbom_format: cyclonedx-json
sbom_path: $sbom_path
sbom_sha256: $(ctd_sha256 "$sbom_path")
sbom_artifact_digest: $sbom_artifact_digest
sbom_binary_digest: sha256:$BINARY_SHA
license_report_path: $LICENSE_REPORT
license_report_sha256: $(ctd_sha256 "$LICENSE_REPORT")
EOF
}

run_release_sbom_gate() {
  local receipt="$1"
  local sbom_path="$2"
  local license_path="$3"
  SBOM_DOC="$SBOM_POLICY" bash "$REPO/patches/transportd/tools/sbom-gate.sh" \
    --receipt "$receipt" --artifact "$ARTIFACT" --binary "$BINARY" \
    --sbom "$sbom_path" --license-report "$license_path" --release
}

write_receipt "$RECEIPT" "$SBOM" "sha256:$ARTIFACT_SHA"
run_release_sbom_gate "$RECEIPT" "$SBOM" "$LICENSE_REPORT" >/dev/null

EMPTY_SBOM="$WORK/empty.cdx.json"
python3 - "$SBOM" "$EMPTY_SBOM" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    sbom = json.load(handle)
sbom["components"] = []
with open(sys.argv[2], "w", encoding="utf-8") as handle:
    json.dump(sbom, handle)
PY
EMPTY_RECEIPT="$WORK/empty-receipt.txt"
write_receipt "$EMPTY_RECEIPT" "$EMPTY_SBOM" "sha256:$ARTIFACT_SHA"
if run_release_sbom_gate "$EMPTY_RECEIPT" "$EMPTY_SBOM" "$LICENSE_REPORT" >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted an empty component inventory" >&2
  exit 1
fi

MALFORMED_SBOM="$WORK/malformed.cdx.json"
printf '{\n' >"$MALFORMED_SBOM"
MALFORMED_RECEIPT="$WORK/malformed-receipt.txt"
write_receipt "$MALFORMED_RECEIPT" "$MALFORMED_SBOM" "sha256:$ARTIFACT_SHA"
if run_release_sbom_gate "$MALFORMED_RECEIPT" "$MALFORMED_SBOM" "$LICENSE_REPORT" >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted malformed JSON" >&2
  exit 1
fi

MUTATED_SBOM="$WORK/mutated.cdx.json"
cp "$SBOM" "$MUTATED_SBOM"
write_receipt "$WORK/mutated-receipt.txt" "$MUTATED_SBOM" "sha256:$ARTIFACT_SHA"
printf ' ' >>"$MUTATED_SBOM"
if run_release_sbom_gate "$WORK/mutated-receipt.txt" "$MUTATED_SBOM" "$LICENSE_REPORT" >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted changed SBOM bytes" >&2
  exit 1
fi

MISMATCH_RECEIPT="$WORK/mismatch-receipt.txt"
write_receipt "$MISMATCH_RECEIPT" "$SBOM" "sha256:$(printf '0%.0s' {1..64})"
if run_release_sbom_gate "$MISMATCH_RECEIPT" "$SBOM" "$LICENSE_REPORT" >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted an artifact digest mismatch" >&2
  exit 1
fi

if run_release_sbom_gate "$RECEIPT" "$SBOM" "$WORK/missing-license.json" >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted a missing license report" >&2
  exit 1
fi

UNVERIFIED_DOC="$WORK/SBOM.unverified.md"
sed 's/^sbom_artifact_status: .*/sbom_artifact_status: unverified/' \
  "$SBOM_POLICY" >"$UNVERIFIED_DOC"
if SBOM_DOC="$UNVERIFIED_DOC" bash "$REPO/patches/transportd/tools/sbom-gate.sh" \
  --receipt "$RECEIPT" --artifact "$ARTIFACT" --binary "$BINARY" \
  --sbom "$SBOM" --license-report "$LICENSE_REPORT" --release >/dev/null 2>&1; then
  echo "FAIL: SBOM gate accepted unverified artifact policy" >&2
  exit 1
fi

echo "PASS: wire and SBOM gates reject identity and artifact mutations"
