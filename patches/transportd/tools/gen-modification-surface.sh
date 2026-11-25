#!/usr/bin/env bash
# 生成 fork 的完整修改面清单（机器生成，禁止手抄）。
#
# 输入：patches/transportd/series.yaml 的 peeled_base_commit（upstream 基线）与
#       source_commit（本线产品提交）——两者都从实际链读出，不写死。
# 输出：patches/transportd/modified-paths.txt（排序后逐行一个路径，头部为分组统计）。
#
# 等价命令：git diff --name-only <peeled_base_commit> <source_commit>
#
# PATCHES.md 的 P-row 表只是需求映射的子集；本清单才是完整修改面。
set -euo pipefail

# shellcheck source=patches/transportd/tools/identity-lib.sh
source "$(dirname "$0")/identity-lib.sh"
REPO="$(ctd_repo_root)"
MANIFEST="$REPO/patches/transportd/series.yaml"
OUT="$REPO/patches/transportd/modified-paths.txt"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

BASE="$(ctd_field "$MANIFEST" peeled_base_commit)"
SOURCE="$(ctd_field "$MANIFEST" source_commit)"
ctd_require_hex peeled_base_commit "$BASE" 40
ctd_require_hex source_commit "$SOURCE" 40
git -C "$REPO" cat-file -e "$BASE^{commit}" 2>/dev/null || fail "base commit is unavailable: $BASE"
git -C "$REPO" cat-file -e "$SOURCE^{commit}" 2>/dev/null || fail "source commit is unavailable: $SOURCE"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-modified-surface.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
git -C "$REPO" diff --name-only "$BASE" "$SOURCE" | LC_ALL=C sort >"$WORK/paths"

TOTAL="$(wc -l <"$WORK/paths" | tr -d ' ')"
TRANSPORTD="$(grep -c '^codex-rs/codex-transportd/' "$WORK/paths" || true)"
CODEX_OTHER="$(grep '^codex-rs/' "$WORK/paths" | grep -vc '^codex-rs/codex-transportd/' || true)"
NON_CODEX="$(grep -vc '^codex-rs/' "$WORK/paths" || true)"
[ "$((TRANSPORTD + CODEX_OTHER + NON_CODEX))" -eq "$TOTAL" ] || fail "path partition does not add up"

{
  echo "# fork 完整修改面（机器生成；请勿手改）"
  echo "# 生成命令：bash patches/transportd/tools/gen-modification-surface.sh"
  echo "# 等价命令：git diff --name-only $BASE $SOURCE"
  echo "# upstream 基线（peeled_base_commit）：$BASE"
  echo "# 本线产品提交（source_commit）：$SOURCE"
  echo "# 路径总数：${TOTAL}（codex-rs/codex-transportd/ ${TRANSPORTD}；codex-rs/ 其它 ${CODEX_OTHER}；非 codex-rs/ ${NON_CODEX}）"
  cat "$WORK/paths"
} >"$OUT"

echo "PASS: wrote $OUT ($TOTAL paths)"
