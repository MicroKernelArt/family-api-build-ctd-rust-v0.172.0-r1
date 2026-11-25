#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"

[ "$#" -eq 1 ] || {
  echo "FAIL: usage: write-source-receipt.sh /absolute/path/outside/repository" >&2
  exit 1
}
RECEIPT="$1"
ctd_require_external_receipt "$REPO" "$RECEIPT"

SOURCE_COMMIT="$(ctd_field "$MANIFEST" source_commit)"
SOURCE_TREE="$(ctd_field "$MANIFEST" source_tree)"
RELEASE_ID="$(ctd_field "$MANIFEST" release_id)"
REVIEW_COMMIT="$(git -C "$REPO" rev-parse HEAD)"
REVIEW_TREE="$(git -C "$REPO" rev-parse 'HEAD^{tree}')"

if command -v shasum >/dev/null 2>&1; then
  ARCHIVE_SHA="$(git -C "$REPO" archive "$SOURCE_COMMIT" | shasum -a 256 | awk '{print $1}')"
else
  ARCHIVE_SHA="$(git -C "$REPO" archive "$SOURCE_COMMIT" | sha256sum | awk '{print $1}')"
fi

mkdir -p "$(dirname "$RECEIPT")"
cat >"$RECEIPT" <<EOF
schema: ctd/evidence-receipt/v2
artifact_kind: source-archive
artifact_digest: sha256:$ARCHIVE_SHA
source_commit: $SOURCE_COMMIT
source_tree: $SOURCE_TREE
review_commit: $REVIEW_COMMIT
review_tree: $REVIEW_TREE
release_id: $RELEASE_ID
EOF
echo "PASS: wrote external source receipt $RECEIPT"
