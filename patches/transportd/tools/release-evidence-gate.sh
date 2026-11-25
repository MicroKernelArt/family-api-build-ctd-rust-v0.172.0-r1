#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"
RECEIPT=""
ARTIFACT=""
RELEASE_MODE=0

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --receipt)
      [ "$#" -ge 2 ] || fail "--receipt requires a path"
      RECEIPT="$2"
      shift 2
      ;;
    --release)
      RELEASE_MODE=1
      shift
      ;;
    --artifact)
      [ "$#" -ge 2 ] || fail "--artifact requires a path"
      ARTIFACT="$2"
      shift 2
      ;;
    *) fail "unknown argument: $1" ;;
  esac
done
[ -n "$RECEIPT" ] || fail "--receipt is required"
ctd_require_external_receipt "$REPO" "$RECEIPT"
[ -f "$RECEIPT" ] || fail "receipt does not exist: $RECEIPT"

SOURCE_COMMIT="$(ctd_field "$MANIFEST" source_commit)"
SOURCE_TREE="$(ctd_field "$MANIFEST" source_tree)"
RELEASE_ID="$(ctd_field "$MANIFEST" release_id)"
CARGO_LOCK_SHA="$(ctd_field "$MANIFEST" cargo_lock_sha256)"
BAZEL_LOCK_SHA="$(ctd_field "$MANIFEST" module_bazel_lock_sha256)"
RECEIPT_SOURCE="$(ctd_field "$RECEIPT" source_commit)"
RECEIPT_TREE="$(ctd_field "$RECEIPT" source_tree)"
RECEIPT_REVIEW="$(ctd_field "$RECEIPT" review_commit)"
RECEIPT_REVIEW_TREE="$(ctd_field "$RECEIPT" review_tree)"
RECEIPT_RELEASE_ID="$(ctd_field "$RECEIPT" release_id)"
ARTIFACT_KIND="$(ctd_field "$RECEIPT" artifact_kind)"
ARTIFACT_DIGEST="$(ctd_field "$RECEIPT" artifact_digest)"

[ "$(ctd_field "$RECEIPT" schema)" = "ctd/evidence-receipt/v2" ] || fail "unsupported receipt schema"
[ "$RECEIPT_SOURCE" = "$SOURCE_COMMIT" ] || fail "receipt source_commit mismatch"
[ "$RECEIPT_TREE" = "$SOURCE_TREE" ] || fail "receipt source_tree mismatch"
[ "$RECEIPT_RELEASE_ID" = "$RELEASE_ID" ] || fail "receipt release_id mismatch"
[ "$(git -C "$REPO" rev-parse "$SOURCE_COMMIT^{tree}")" = "$SOURCE_TREE" ] || fail "manifest source identity mismatch"
ctd_require_hex receipt_review_commit "$RECEIPT_REVIEW" 40
ctd_require_hex receipt_review_tree "$RECEIPT_REVIEW_TREE" 40
[ "$(git -C "$REPO" rev-parse "$RECEIPT_REVIEW^{tree}")" = "$RECEIPT_REVIEW_TREE" ] || fail "receipt review identity mismatch"
case "$ARTIFACT_DIGEST" in
  sha256:*) ctd_require_hex artifact_digest "${ARTIFACT_DIGEST#sha256:}" 64 ;;
  *) fail "artifact_digest must use sha256" ;;
esac
[ "$(git -C "$REPO" show "$SOURCE_COMMIT:codex-rs/Cargo.lock" | ctd_sha256 /dev/stdin)" = "$CARGO_LOCK_SHA" ] || fail "source Cargo.lock mismatch"
[ "$(git -C "$REPO" show "$SOURCE_COMMIT:MODULE.bazel.lock" | ctd_sha256 /dev/stdin)" = "$BAZEL_LOCK_SHA" ] || fail "source MODULE.bazel.lock mismatch"

if [ "$ARTIFACT_KIND" = "source-archive" ]; then
  if command -v shasum >/dev/null 2>&1; then
    EXPECTED="$(git -C "$REPO" archive "$SOURCE_COMMIT" | shasum -a 256 | awk '{print $1}')"
  else
    EXPECTED="$(git -C "$REPO" archive "$SOURCE_COMMIT" | sha256sum | awk '{print $1}')"
  fi
  [ "$ARTIFACT_DIGEST" = "sha256:$EXPECTED" ] || fail "source archive digest mismatch"
fi

if [ "$RELEASE_MODE" -eq 1 ]; then
  [ "$ARTIFACT_KIND" = "oci-image" ] || fail "release receipt must describe an oci-image"
  [ -n "$ARTIFACT" ] || fail "release verification requires --artifact"
  ctd_require_external_receipt "$REPO" "$ARTIFACT"
  [ -s "$ARTIFACT" ] || fail "release artifact is missing or empty: $ARTIFACT"
  [ "$(ctd_field "$RECEIPT" artifact_format)" = "docker-archive" ] || {
    fail "release artifact must be a docker-archive"
  }
  [ "$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$ARTIFACT")" = \
    "$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$(ctd_field "$RECEIPT" artifact_path)")" ] || {
    fail "release artifact path does not match receipt"
  }
  [ "$ARTIFACT_DIGEST" = "sha256:$(ctd_sha256 "$ARTIFACT")" ] || {
    fail "release artifact digest mismatch"
  }
  [ "$(ctd_field "$RECEIPT" build_profile)" = "release" ] || fail "release receipt profile mismatch"
  [ "$(ctd_field "$RECEIPT" build_mode)" = "local-cargo" ] || fail "release receipt build mode mismatch"
  [ "$(ctd_field "$RECEIPT" platform)" = "linux/amd64" ] || fail "release receipt platform mismatch"
  RELEASE_TAG="$(ctd_field "$MANIFEST" release_tag)"
  TAG_COMMIT="$(git -C "$REPO" rev-list -n 1 "$RELEASE_TAG" 2>/dev/null || true)"
  [ "$TAG_COMMIT" = "$RECEIPT_REVIEW" ] || fail "release tag does not identify receipt review_commit"
fi

CURRENT_REVIEW="$(git -C "$REPO" rev-parse HEAD)"
CURRENT_REVIEW_TREE="$(git -C "$REPO" rev-parse 'HEAD^{tree}')"
echo "PASS: source identity $SOURCE_COMMIT $SOURCE_TREE"
echo "PASS: artifact identity $ARTIFACT_KIND $ARTIFACT_DIGEST"
echo "PASS: receipt review identity $RECEIPT_REVIEW $RECEIPT_REVIEW_TREE"
echo "PASS: gate review identity $CURRENT_REVIEW $CURRENT_REVIEW_TREE"
echo "PASS: release identity $RELEASE_ID"
