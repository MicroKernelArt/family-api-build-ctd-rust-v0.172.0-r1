#!/usr/bin/env bash
# R-PLATFORM-001（replay_0831 R6）：Windows 全 workspace 档案的平台策略
# gate——可执行、离线、确定性。
#
# 断言：
#   1. codex-transportd 是无条件 workspace 成员；
#   2. 该 crate 含非 cfg 门控的 Unix-only API（排除有据，不伪装支持）；
#   3. rust-ci-full-nextest-platform.yml 的 Windows 目标在 nextest 档案中
#      显式排除 codex-transportd；
#   4. 排除是条件性的：受支持的 Unix 目标档案不排除（保持完整覆盖）；
#   5. Windows 排除清单只含被 (2) 证明的 crate。
#
# 用法: check-platform-archives.sh    （任何断言失败 → exit 1）
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
MANIFEST="$REPO/codex-rs/Cargo.toml"
WF="$REPO/.github/workflows/rust-ci-full-nextest-platform.yml"
FULL_WORKFLOW="$REPO/.github/workflows/rust-ci-full.yml"
ARCHIVE_WORKFLOW="$REPO/.github/workflows/rust-ci-full-nextest-platform.yml"
CRATE_DIR="$REPO/codex-rs/codex-transportd"
FAILS=0

ok() { echo "  ok: $1"; }
bad() {
  echo "FAIL: $1"
  FAILS=1
}

require_file() {
  [ -f "$1" ] || { echo "FAIL: required archive policy input is missing: $1" >&2; exit 1; }
}

require_text() {
  local file="$1"
  local needle="$2"
  grep -qF -- "$needle" "$file" || { echo "FAIL: $file is missing: $needle" >&2; exit 1; }
}

echo "== 平台归档策略 gate（R-PLATFORM-001） =="

# 1. workspace 成员资格无条件。
if grep -qF '"codex-transportd"' "$MANIFEST"; then
  ok "codex-transportd 是 workspace 成员"
else
  bad "codex-rs/Cargo.toml 缺少 codex-transportd 成员"
fi

# 2. 排除依据：crate 内非 cfg 门控的 Unix-only API。
UNIX_MARKERS=0
if grep -qE '^use tokio::net::UnixListener;' "$CRATE_DIR/src/server/mod.rs"; then
  ok "server/mod.rs 含无条件 UnixListener import"
  UNIX_MARKERS=$((UNIX_MARKERS + 1))
else
  bad "预期 server/mod.rs 的无条件 UnixListener import 缺失（crate 可能已跨平台，策略需复核）"
fi
if grep -qE '^use std::os::fd::' "$CRATE_DIR/src/capture_writer.rs"; then
  ok "capture_writer.rs 含无条件 Unix fd import"
  UNIX_MARKERS=$((UNIX_MARKERS + 1))
else
  bad "预期 capture_writer.rs 的无条件 Unix fd import 缺失（crate 可能已跨平台，策略需复核）"
fi

# 3+4. Windows 目标条件性排除；Unix 目标不排除。
[ -f "$WF" ] || bad "workflow 不存在: $WF"
if grep -qF 'archive_excludes=()' "$WF"; then
  ok "档案排除数组存在（默认为空 = Unix 全覆盖）"
else
  bad "workflow 缺少 archive_excludes=() 初始化"
fi
if grep -qF -- '*-pc-windows-msvc) archive_excludes+=(--exclude codex-transportd) ;;' "$WF"; then
  ok "Windows 目标显式排除 codex-transportd"
else
  bad "workflow 缺少 Windows 分支的 --exclude codex-transportd"
fi
if grep -qF "\"\${archive_excludes[@]}\"" "$WF"; then
  ok "cargo nextest archive 消费排除数组"
else
  bad "cargo nextest archive 调用未消费 \"\${archive_excludes[@]}\""
fi

# 5. 排除清单最小性：--exclude 只允许出现在 Windows case 分支一行。
EXCLUDE_LINES="$(grep -c -- '--exclude codex-transportd' "$WF" || true)"
if [ "$EXCLUDE_LINES" -eq 1 ]; then
  ok "排除只声明一次（无无条件排除、无范围蔓延）"
else
  bad "--exclude codex-transportd 出现 $EXCLUDE_LINES 次（必须仅 Windows case 分支 1 次）"
fi

if [ "$FAILS" -eq 0 ]; then
  echo "PASS: 平台归档策略（unix-only crate 明确退出 Windows 全 workspace 档案；Unix 检查保持完整）"
else
  exit 1
fi

# Worker CI additionally checks every declared target/artifact and fixed archive wiring.
require_file "$FULL_WORKFLOW"
require_file "$ARCHIVE_WORKFLOW"

targets=(
  aarch64-apple-darwin
  x86_64-apple-darwin
  x86_64-unknown-linux-musl
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-musl
  aarch64-unknown-linux-gnu
  x86_64-pc-windows-msvc
  aarch64-pc-windows-msvc
)
for target in "${targets[@]}"; do
  require_text "$FULL_WORKFLOW" "target: $target"
done

archive_pairs=(
  'aarch64-apple-darwin|macos-aarch64'
  'x86_64-unknown-linux-gnu|linux-x64-remote'
  'aarch64-unknown-linux-gnu|linux-arm64'
  'x86_64-pc-windows-msvc|windows-x64'
  'aarch64-pc-windows-msvc|windows-arm64'
)
for pair in "${archive_pairs[@]}"; do
  target="${pair%%|*}"
  artifact="${pair#*|}"
  require_text "$FULL_WORKFLOW" "target: $target"
  require_text "$FULL_WORKFLOW" "artifact_id: $artifact"
done
require_text "$FULL_WORKFLOW" 'archive_runner: windows-x64'

require_text "$ARCHIVE_WORKFLOW" "NEXTEST_ARCHIVE_FILE: nextest-\${{ inputs.artifact_id }}.tar.zst"
require_text "$ARCHIVE_WORKFLOW" "--archive-file \"\${archive_dir}/\${NEXTEST_ARCHIVE_FILE}\""
require_text "$ARCHIVE_WORKFLOW" "name: nextest-archive-\${{ inputs.artifact_id }}"
require_text "$ARCHIVE_WORKFLOW" "path: \${{ runner.temp }}/nextest-archive/\${{ env.NEXTEST_ARCHIVE_FILE }}"
require_text "$ARCHIVE_WORKFLOW" 'Download nextest archive'
require_text "$ARCHIVE_WORKFLOW" "name: nextest-archive-\${{ inputs.artifact_id }}"

if grep -Eq 'NEXTEST_ARCHIVE_FILE:.*\.(zip|tar\.gz|tgz)([[:space:]]|$)' "$ARCHIVE_WORKFLOW"; then
  echo 'FAIL: nextest archive policy allows a non-tar.zst archive' >&2
  exit 1
fi

echo "PASS: worker archive identity policy (${RUNNER_OS:-${OSTYPE:-unknown}})"
