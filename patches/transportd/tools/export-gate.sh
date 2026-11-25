#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
PATCHES_DIR="${PATCHES_DIR:-$REPO/patches/transportd}"
MANIFEST="$PATCHES_DIR/series.yaml"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

[ "$#" -eq 0 ] || fail "export-gate accepts no arguments"
[ -f "$MANIFEST" ] || fail "series manifest is missing: $MANIFEST"

BASE="$(ctd_field "$MANIFEST" peeled_base_commit)"
SOURCE_COMMIT="$(ctd_field "$MANIFEST" source_commit)"
SOURCE_TREE="$(ctd_field "$MANIFEST" source_tree)"
EXPECTED_RELEASE_TREE="$(ctd_field "$MANIFEST" expected_release_tree)"
EXPECTED_COUNT="$(ctd_field "$MANIFEST" series_entries)"
EXPECTED_CARGO_LOCK="$(ctd_field "$MANIFEST" cargo_lock_sha256)"
EXPECTED_BAZEL_LOCK="$(ctd_field "$MANIFEST" module_bazel_lock_sha256)"
ctd_require_hex peeled_base_commit "$BASE" 40
ctd_require_hex source_commit "$SOURCE_COMMIT" 40
ctd_require_hex source_tree "$SOURCE_TREE" 40
ctd_require_hex expected_release_tree "$EXPECTED_RELEASE_TREE" 40
ctd_require_hex cargo_lock_sha256 "$EXPECTED_CARGO_LOCK" 64
ctd_require_hex module_bazel_lock_sha256 "$EXPECTED_BAZEL_LOCK" 64
case "$EXPECTED_COUNT" in
  ''|*[!0-9]*) fail "series_entries must be a positive integer" ;;
esac
[ "$EXPECTED_COUNT" -gt 0 ] || fail "series_entries must be positive"

git -C "$REPO" cat-file -e "$BASE^{commit}" 2>/dev/null || fail "base commit is unavailable: $BASE"
git -C "$REPO" cat-file -e "$SOURCE_COMMIT^{commit}" 2>/dev/null || fail "source commit is unavailable: $SOURCE_COMMIT"
git -C "$REPO" merge-base --is-ancestor "$BASE" "$SOURCE_COMMIT" || fail "base is not an ancestor of source"
[ "$(git -C "$REPO" rev-parse "$SOURCE_COMMIT^{tree}")" = "$SOURCE_TREE" ] || fail "source commit/tree mismatch"
[ "$EXPECTED_RELEASE_TREE" = "$SOURCE_TREE" ] || fail "expected release tree does not match source tree"
[ "$(git -C "$REPO" show "$SOURCE_COMMIT:codex-rs/Cargo.lock" | ctd_sha256 /dev/stdin)" = "$EXPECTED_CARGO_LOCK" ] || fail "source Cargo.lock digest mismatch"
[ "$(git -C "$REPO" show "$SOURCE_COMMIT:MODULE.bazel.lock" | ctd_sha256 /dev/stdin)" = "$EXPECTED_BAZEL_LOCK" ] || fail "source MODULE.bazel.lock digest mismatch"

SERIES=()
while IFS= read -r entry; do
  file="${entry#  - }"
  [ -n "$file" ] || fail "empty series entry"
  [ -f "$PATCHES_DIR/$file" ] || fail "series entry is missing: $file"
  SERIES+=("$file")
done < <(grep '^  - full/.*\.patch$' "$MANIFEST")
[ "${#SERIES[@]}" -eq "$EXPECTED_COUNT" ] || fail "series entry count mismatch"

actual_patches="$(find "$PATCHES_DIR/full" -maxdepth 1 -type f -name '*.patch' | wc -l | tr -d ' ')"
[ "$actual_patches" -eq "$EXPECTED_COUNT" ] || fail "unlisted or missing patch files"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-export.XXXXXX")"
REPLAY="$WORK/replay"
cleanup() {
  git -C "$REPO" worktree remove --force "$REPLAY" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

git -C "$REPO" worktree add --detach "$REPLAY" "$BASE" >/dev/null
PATCH_PATHS=()
for file in "${SERIES[@]}"; do
  PATCH_PATHS+=("$PATCHES_DIR/$file")
done
git -C "$REPLAY" am --quiet "${PATCH_PATHS[@]}"

REPLAY_TREE="$(git -C "$REPLAY" rev-parse 'HEAD^{tree}')"
[ "$REPLAY_TREE" = "$SOURCE_TREE" ] || {
  git -C "$REPLAY" diff --stat "$SOURCE_TREE" HEAD >&2 || true
  fail "exported tree $REPLAY_TREE does not equal declared source tree $SOURCE_TREE"
}

REVIEW_COMMIT="$(git -C "$REPO" rev-parse HEAD)"
REVIEW_TREE="$(git -C "$REPO" rev-parse 'HEAD^{tree}')"
echo "PASS: source_commit=$SOURCE_COMMIT source_tree=$SOURCE_TREE"
echo "PASS: exported_tree=$REPLAY_TREE patches=${#SERIES[@]}"
echo "PASS: review_commit=$REVIEW_COMMIT review_tree=$REVIEW_TREE"
