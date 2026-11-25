#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="${REPLAY_MANIFEST:-$REPO/replay/manifest.yaml}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

field() {
  # YAML 行内注释不属于值：先去掉 " #..." 再返回。
  sed -n "s/^$1:[[:space:]]*//p" "$MANIFEST" | head -1 | sed "s/[[:space:]][[:space:]]*#.*$//"
}

[ "$#" -eq 0 ] || fail "replay gate accepts no arguments"
BASE="$(field base_commit)"
SOURCE_COMMIT="$(field source_commit)"
SOURCE_TREE="$(field source_tree)"
RELEASE_COMMIT="$(field release_commit)"
EXPECTED_RELEASE_TREE="$(field expected_release_tree)"
EXPECTED_COUNT="$(field commit_count)"
# 产品提交处的 manifest 只有头部（source/release 与逐提交清单是自引用的，由证据提交生成）。
# 显式声明 partial 时跳过这些字段的校验，而不是误报 "source commit is unavailable"。
MANIFEST_STATUS="$(field manifest_status)"
case "$MANIFEST_STATUS" in
  partial-*)
    echo "SKIP: manifest_status=${MANIFEST_STATUS}（产品提交处 manifest 仅头部；source/release 由证据提交生成）"
    exit 0 ;;
esac
git -C "$REPO" cat-file -e "$BASE^{commit}" 2>/dev/null || fail "base commit is unavailable"
git -C "$REPO" cat-file -e "$SOURCE_COMMIT^{commit}" 2>/dev/null || fail "source commit is unavailable"
git -C "$REPO" cat-file -e "$RELEASE_COMMIT^{commit}" 2>/dev/null || fail "release commit is unavailable"
[ "$(git -C "$REPO" rev-parse "$SOURCE_COMMIT^{tree}")" = "$SOURCE_TREE" ] || fail "source commit/tree mismatch"
[ "$(git -C "$REPO" rev-parse "$RELEASE_COMMIT^{tree}")" = "$EXPECTED_RELEASE_TREE" ] || fail "release commit/tree mismatch"
git -C "$REPO" merge-base --is-ancestor "$SOURCE_COMMIT" "$RELEASE_COMMIT" || {
  fail "source_commit is not an ancestor of release_commit"
}

COMMITS=()
TREES=()
while IFS= read -r commit; do
  COMMITS+=("$commit")
done < <(sed -n 's/^  - commit: //p' "$MANIFEST")
while IFS= read -r tree; do
  TREES+=("$tree")
done < <(sed -n 's/^    tree: //p' "$MANIFEST")
[ "${#COMMITS[@]}" -eq "$EXPECTED_COUNT" ] || fail "commit count mismatch"
[ "${#TREES[@]}" -eq "$EXPECTED_COUNT" ] || fail "tree count mismatch"

PREVIOUS="$BASE"
for index in "${!COMMITS[@]}"; do
  commit="${COMMITS[$index]}"
  tree="${TREES[$index]}"
  git -C "$REPO" cat-file -e "$commit^{commit}" 2>/dev/null || fail "replay commit is unavailable: $commit"
  [ "$(git -C "$REPO" rev-parse "$commit^")" = "$PREVIOUS" ] || fail "replay line is not contiguous at $commit"
  [ "$(git -C "$REPO" rev-parse "$commit^{tree}")" = "$tree" ] || fail "replay tree mismatch at $commit"
  PREVIOUS="$commit"
done
[ "$PREVIOUS" = "$SOURCE_COMMIT" ] || fail "last replay commit is not source_commit"

CURRENT="$(git -C "$REPO" rev-parse HEAD)"
git -C "$REPO" merge-base --is-ancestor "$RELEASE_COMMIT" "$CURRENT" || fail "review HEAD does not contain release_commit"
for commit in $(git -C "$REPO" rev-list "$SOURCE_COMMIT..$CURRENT" --no-merges); do
  while IFS= read -r path; do
    case "$path" in
      PATCHES.md|SBOM.md|UPSTREAM_BASE.md|WIRE_COMPATIBILITY.md|codex-rs/codex-transportd/BUILD_PROOF.md|patches/transportd/*|replay/manifest.yaml) ;;
      *)
        # 产品路径的 post-source 例外必须在 manifest 里显式声明（post_source_allow，空格分隔）。
        # 例外要可见、可审计，而不是靠放宽通用白名单。
        _allowed=0
        for _a in $(field post_source_allow); do
          [ "$path" = "$_a" ] && { _allowed=1; break; }
        done
        [ "$_allowed" = 1 ] || fail "post-source review commit changes non-evidence path: $commit $path"
        ;;
    esac
  done < <(git -C "$REPO" show --format= --name-only "$commit")
done

echo "PASS: replay source=$SOURCE_COMMIT tree=$SOURCE_TREE commits=$EXPECTED_COUNT"
echo "PASS: review=$CURRENT tree=$(git -C "$REPO" rev-parse 'HEAD^{tree}')"
