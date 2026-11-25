#!/usr/bin/env bash
# build-sidecar smoke volume ownership 回归测试（R18-F2/FXF-0001）。
# 独立于生产入口：生产函数来自 deploy/smoke-volume-lib.sh（source），
# 生产编排通过运行真实 `build --mode local-cargo`（owned mock preflight）
# 覆盖。由 build-sidecar.sh self-test-volume 委托执行；release-evidence-gate
# 经该入口调用。用法:
#   build-sidecar-volume-test.sh self-test-volume
#   build-sidecar-volume-test.sh self-test-volume-cache-probe   # 场景 E 私有子模式
set -euo pipefail

TEST_REPO="$(cd "$(dirname "$0")/.." && pwd)"
source "$TEST_REPO/deploy/smoke-volume-lib.sh"
ST_SCRIPT="$TEST_REPO/deploy/build-sidecar.sh"
ST_REPO="$TEST_REPO"

# mock docker：behavior（ok|fail）生成时内插；raw 调用行与 create 名入日志。
st_mock_docker() {
  local dir="$1" behavior="$2"
  cat > "$dir/docker" <<EOF
#!/usr/bin/env bash
echo "docker \$*" >> "\$ST_DOCKER_LOG"
case "\$1 \$2" in
"info --format")
  echo x86_64
  ;;
"volume create")
  if [ "$behavior" = "fail" ]; then
    echo "mock: create forced fail" >&2
    exit 1
  fi
  if [ "\$#" -ne 2 ]; then
    echo "mock: volume create 必须为 2 参匿名调用（named 回归被拒）" >&2
    exit 1
  fi
  ST_NAME="st-smoke-\$RANDOM\$RANDOM"
  echo "docker volume create \$ST_NAME" >> "\$ST_DOCKER_LOG"
  echo "\$ST_NAME"
  ;;
"volume rm")
  echo "rm \${@: -1}" >> "\$ST_RM_LOG"
  ;;
esac
exit 0
EOF
  chmod +x "$dir/docker"
}

# 真实 build 编排执行器：以完全 owned 的 mock preflight（docker/git/shasum +
# 临时 v8 cache + PLATFORM）运行 `build --mode local-cargo`，确定性停在
# step 0 的 dirty-worktree 早失败（create 之后）。覆盖生产 trap 安装、
# cleanup 委托、callsite 与 fail-closed 全链。ignore_error 经参数传递。
st_mock_git() {
  local dir="$1"
  # 自包含 mock：记录全部调用；status 输出 dirty marker（触发 step 0 早失败）。
  # 其余 git 调用不会到达（step 0 在 dirty 检查处 exit）。不依赖主机 git 存在。
  cat > "$dir/git" <<'EOF'
#!/usr/bin/env bash
echo "git $*" >> "$ST_GIT_LOG"
if [ "$1" = "status" ]; then
echo " M self-test-dirty-marker"
exit 0
fi
exit 0
EOF
  chmod +x "$dir/git"
}
st_mock_shasum() {
  local dir="$1"
  cat > "$dir/shasum" <<EOF
#!/usr/bin/env bash
# 输出 series.yaml 声明的期望 sha——v8 归档内容无关（mock preflight）
sed -n 's/^v8_archive_sha256_x86_64-unknown-linux-gnu: //p' "$ST_REPO/patches/transportd/series.yaml" | tr -d ' ' | sed "s/\$/  \$1/"
EOF
  chmod +x "$dir/shasum"
}
st_lifecycle() {
  local behavior="$1"
  local dir="$ST_WORK/$behavior"
  (
    mkdir -p "$ST_WORK/v8"
    : > "$ST_WORK/v8/librusty_v8_release_x86_64-unknown-linux-gnu.a.gz"
    # env -i：子 build 只拿到显式声明的输入——消除 CARGO_TARGET_CACHE 等
    # 环境泄漏（M2），也不继承任何主机状态。
    env -i \
      PATH="$dir:$PATH" HOME="$ST_WORK" TMPDIR="${TMPDIR:-/tmp}" LC_ALL=C \
      PLATFORM=linux/amd64 CTD_V8_CACHE="$ST_WORK/v8" \
      CTD_RELEASE_RECEIPT_PATH="$ST_WORK/release-receipt.txt" \
      ST_DOCKER_LOG="$ST_DOCKER_LOG" ST_RM_LOG="$ST_RM_LOG" ST_GIT_LOG="$ST_GIT_LOG" \
      bash "$ST_SCRIPT" build --mode local-cargo >"$ST_WORK/$behavior.log" 2>&1
  )
}




# ---------- self-test-volume-cache-probe（私有子模式） ----------
# 由 self-test-volume 的场景 E 调用：本进程环境的 CARGO_TARGET_CACHE 是
# hostile 注入值；子 build 经 st_lifecycle 的 env -i 必须隔离它。公开入口
# self-test-volume 始终执行场景 E（无环境守卫——ST_E_RAN 类可跳过守卫已移除）。
if [ "${1:-}" = "self-test-volume-cache-probe" ]; then
  [ "$#" -le 1 ] || { echo "FAIL: self-test-volume-cache-probe 不接受多余参数"; exit 1; }
  ST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-st-probe.XXXXXX")"
  trap 'rm -rf "$ST_WORK"' EXIT
  # ST_SCRIPT/ST_REPO 已在文件顶部指向生产脚本/仓库——probe 子 build 必须
  # 运行生产 build-sidecar.sh（st_lifecycle 的 env -i 隔离）。
  ST_PROBE_PASS=1
  ST_DOCKER_LOG="$ST_WORK/p.docker" ST_RM_LOG="$ST_WORK/p.rm" ST_GIT_LOG="$ST_WORK/p.gitlog"
  : > "$ST_DOCKER_LOG"; : > "$ST_RM_LOG"; : > "$ST_GIT_LOG"
  mkdir -p "$ST_WORK/ok"
  st_mock_docker "$ST_WORK/ok" ok
  st_mock_git "$ST_WORK/ok"; st_mock_shasum "$ST_WORK/ok"
  set +e
  st_lifecycle ok
  ST_P_RC=$?
  set -e
  ST_P_CREATE2="$(grep -c '^docker volume create$' "$ST_DOCKER_LOG" || true)"
  ST_P_SENTINEL_CREATED=0
  if [ -n "${ST_PROBE_SENTINEL:-}" ] && [ -e "$ST_PROBE_SENTINEL" ]; then
    ST_P_SENTINEL_CREATED=1
  fi
  if [ "$ST_P_RC" = 0 ]; then echo "FAIL probe: 子 build 未在早失败处退出（rc=0）"; ST_PROBE_PASS=0; fi
  if [ "$ST_P_CREATE2" != 1 ]; then echo "FAIL probe: hostile cache 下未到达 volume 场景（create=${ST_P_CREATE2}）"; ST_PROBE_PASS=0; fi
  if [ "$ST_P_SENTINEL_CREATED" = 1 ]; then echo "FAIL probe: hostile cache 泄漏——哨兵被创建"; ST_PROBE_PASS=0; fi
  exit $((1 - ST_PROBE_PASS))
fi


# ---------- self-test-volume（R18-F2/FXF-0001 回归） ----------
# 放在 PLATFORM/v8 preflight 之前：hermetic，不依赖任何主机状态/环境变量。
# 直接驱动生产 callsite（smoke_volume_setup + release_smoke_volume）。
#   A：匿名 create + 早失败 → 恰好一次 rm（rm 名 == create 返回名，raw 2 参匿名）。
#   B：create 失败 → fail closed → 零 rm + 失败后无任何 docker 命令。
#   C（mutation）：callsite 回归 named create → strict mock 拒绝 → 检测生效。
#   D（mutation）：callsite 忽略 create 错误继续 → 检测生效。
if [ "${1:-}" = "self-test-volume" ]; then
  [ "$#" -le 1 ] || { echo "FAIL: self-test-volume 不接受多余参数"; exit 1; }
  ST_WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-st.XXXXXX")"
  trap 'rm -rf "$ST_WORK"' EXIT
  # ST_SCRIPT/ST_REPO 已在文件顶部指向生产脚本/仓库——st_lifecycle 必须运行
  # 生产 build-sidecar.sh 的真实编排（env -i 隔离）。

  ST_PASS=1

  # ---- 场景 A：真实 build 编排——匿名 create + 早失败 → 一次匹配 rm ----
  ST_DOCKER_LOG="$ST_WORK/a.docker" ST_RM_LOG="$ST_WORK/a.rm" ST_GIT_LOG="$ST_WORK/a.gitlog"
  : > "$ST_DOCKER_LOG"; : > "$ST_RM_LOG"; : > "$ST_GIT_LOG"
  mkdir -p "$ST_WORK/ok"; st_mock_docker "$ST_WORK/ok" ok
  st_mock_git "$ST_WORK/ok"; st_mock_shasum "$ST_WORK/ok"
  set +e
  st_lifecycle ok
  ST_A_RC=$?
  set -e
  ST_A_GIT="$(grep -c '^git ' "$ST_GIT_LOG" || true)"
  ST_A_INFO="$(grep -c '^docker info --format {{.Architecture}}$' "$ST_DOCKER_LOG" || true)"
  ST_A_CREATE2="$(grep -c '^docker volume create$' "$ST_DOCKER_LOG" || true)"
  ST_A_RM="$(grep -c '^rm ' "$ST_RM_LOG" || true)"
  ST_A_NAME="$(grep '^docker volume create st-smoke' "$ST_DOCKER_LOG" | sed 's/^docker volume create //' | tr -d ' ' | head -1 || true)"
  ST_A_RM_NAME="$(sed -n 's/^rm //p' "$ST_RM_LOG" | tr -d ' ' | head -1 || true)"
  if [ "$ST_A_RC" = 0 ]; then echo "FAIL A: 真实 build 未在早失败处退出（rc=0）"; ST_PASS=0; fi
  if [ "$ST_A_INFO" != 1 ]; then echo "FAIL A: native daemon preflight 次数=${ST_A_INFO} != 1"; ST_PASS=0; fi
  if [ "$ST_A_CREATE2" != 1 ]; then echo "FAIL A: 2 参匿名 create 次数=${ST_A_CREATE2} != 1"; ST_PASS=0; fi
  if [ "$ST_A_GIT" != 1 ]; then echo "FAIL A: 早失败应来自 step 0 git 检查（git 活动=${ST_A_GIT} != 1）"; ST_PASS=0; fi
  if [ "$ST_A_RM" != 1 ] || [ -z "$ST_A_NAME" ] || [ "$ST_A_RM_NAME" != "$ST_A_NAME" ]; then
    echo "FAIL A: rm=${ST_A_RM} rm-name=[${ST_A_RM_NAME}] != created=[${ST_A_NAME}]"
    ST_PASS=0
  fi

  # ---- 场景 B：真实 build 编排——create 失败 → fail closed → 零 rm ----
  ST_DOCKER_LOG="$ST_WORK/b.docker" ST_RM_LOG="$ST_WORK/b.rm" ST_GIT_LOG="$ST_WORK/b.gitlog"
  : > "$ST_DOCKER_LOG"; : > "$ST_RM_LOG"; : > "$ST_GIT_LOG"
  mkdir -p "$ST_WORK/fail"; st_mock_docker "$ST_WORK/fail" fail
  st_mock_git "$ST_WORK/fail"; st_mock_shasum "$ST_WORK/fail"
  set +e
  st_lifecycle fail
  ST_B_RC=$?
  set -e
  ST_B_GIT="$(grep -c '^git ' "$ST_GIT_LOG" || true)"
  ST_B_INFO="$(grep -c '^docker info --format {{.Architecture}}$' "$ST_DOCKER_LOG" || true)"
  ST_B_CREATE="$(grep -c '^docker volume create' "$ST_DOCKER_LOG" || true)"
  ST_B_AFTER="$(sed -n '/^docker volume create/,$p' "$ST_DOCKER_LOG" | sed '1d' | grep -c '^docker ' || true)"
  ST_B_RM="$(grep -c '^rm ' "$ST_RM_LOG" || true)"
  if [ "$ST_B_RC" = 0 ]; then echo "FAIL B: create 失败场景未失败（rc=0）"; ST_PASS=0; fi
  if [ "$ST_B_INFO" != 1 ]; then echo "FAIL B: native daemon preflight 次数=${ST_B_INFO} != 1"; ST_PASS=0; fi
  if [ "$ST_B_CREATE" != 1 ]; then echo "FAIL B: create 次数=${ST_B_CREATE} != 1"; ST_PASS=0; fi
  if [ "$ST_B_AFTER" != 0 ]; then echo "FAIL B: create 失败后仍有 docker 命令=${ST_B_AFTER}"; ST_PASS=0; fi
  if [ "$ST_B_GIT" != 0 ]; then echo "FAIL B: create 失败后仍有 git 活动=${ST_B_GIT}（fail-closed 应立即退出）"; ST_PASS=0; fi
  if [ "$ST_B_RM" != 0 ]; then echo "FAIL B: 零 rm 断言失败 rm=${ST_B_RM}"; ST_PASS=0; fi

  # ---- 场景 C（mutation）：callsite 回归 named create 必须被检测 ----
  ST_DOCKER_LOG="$ST_WORK/c.docker" ST_RM_LOG="$ST_WORK/c.rm"
  : > "$ST_DOCKER_LOG"; : > "$ST_RM_LOG"
  mkdir -p "$ST_WORK/c"; st_mock_docker "$ST_WORK/c" ok
  set +e
  (
    # shellcheck disable=SC2030,SC2031 # Each mutation runs in an isolated subshell.
    export PATH="$ST_WORK/c:$PATH" ST_DOCKER_LOG ST_RM_LOG
    trap 'release_smoke_volume' EXIT
    # 回归模拟：callsite 错误使用可预测 named create（strict mock 拒绝）
    SMOKE_VOL=""
    SMOKE_VOL_OWNED=0
    if SMOKE_VOL="$(docker volume create predictable-name)"; then
      SMOKE_VOL_OWNED=1
      return 42
    fi
    return 1
  ) >/dev/null 2>&1
  ST_C_RC=$?
  set -e
  ST_C_NAMED="$(grep -c '^docker volume create predictable-name' "$ST_DOCKER_LOG" || true)"
  ST_C_ANON="$(grep -c '^docker volume create st-smoke' "$ST_DOCKER_LOG" || true)"
  if [ "$ST_C_RC" = 0 ]; then echo "FAIL C(mutation): named create 回归未被拒绝（rc=0）"; ST_PASS=0; fi
  if [ "$ST_C_NAMED" != 1 ]; then echo "FAIL C(mutation): named create 调用未被记录（=${ST_C_NAMED}）"; ST_PASS=0; fi
  if [ "$ST_C_ANON" != 0 ]; then echo "FAIL C(mutation): named create 被当作匿名成功（anon=${ST_C_ANON}）"; ST_PASS=0; fi

  # ---- 场景 D（mutation）：忽略 create 错误继续必须被检测 ----
  # 回归模拟：callsite 失败后仍继续执行 docker 命令（真实 build 路径的
  # fail-closed 被破坏）→ "失败后无命令"不变量击穿 → 检测生效。
  ST_DOCKER_LOG="$ST_WORK/d.docker" ST_RM_LOG="$ST_WORK/d.rm"
  : > "$ST_DOCKER_LOG"; : > "$ST_RM_LOG"
  mkdir -p "$ST_WORK/fail"; st_mock_docker "$ST_WORK/fail" fail
  set +e
  (
    # shellcheck disable=SC2030,SC2031 # Each mutation runs in an isolated subshell.
    export PATH="$ST_WORK/fail:$PATH" ST_DOCKER_LOG ST_RM_LOG
    CID=""; CTX=""; ELF_CHECK_NAME=""; ELF_CHECK_OWNED=0
    WORK_DIR=""; TARGET_OWNED=0; TARGET_DIR=""
    SMOKE_VOL=""; SMOKE_VOL_OWNED=0
    trap cleanup EXIT
    if ! smoke_volume_callsite; then
      docker volume rm -f "ignored-error-vol" >/dev/null 2>&1 || true
      return 1
    fi
    return 42
  ) >/dev/null 2>&1
  ST_D_RC=$?
  set -e
  ST_D_AFTER="$(sed -n '/^docker volume create/,$p' "$ST_DOCKER_LOG" | sed '1d' | grep -c '^docker ' || true)"
  if [ "$ST_D_RC" = 0 ]; then echo "FAIL D(mutation): 忽略 create 错误场景未失败（rc=0）"; ST_PASS=0; fi
  if [ "$ST_D_AFTER" = 0 ]; then
    echo "FAIL D(mutation): 忽略 create 错误未被检测（失败后无命令）"
    ST_PASS=0
  fi

  # ---- 场景 E（对抗 cache 隔离）：env -i 必须隔离 hostile CARGO_TARGET_CACHE ----
  # 无环境守卫：公开入口始终执行。经私有子模式 self-test-volume-cache-probe
  # 注入 hostile cache（可写哨兵 + 不可用 /dev/null）；env -i 若被移除/绕过，
  # 子 build 会创建外部哨兵或 /dev/null 上 mkdir 失败 → probe 失败。
  ST_SENTINEL_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ctd-st-sentinel.XXXXXX")"
  ST_SENTINEL="$ST_SENTINEL_ROOT/cache-target"
  set +e
  # 显式设置 ST_PROBE_SENTINEL：覆盖任何继承值（M1——不可用 cache 子进程
  # 不得把调用方已存在的路径误判为泄漏哨兵）。
  CARGO_TARGET_CACHE="$ST_SENTINEL" ST_PROBE_SENTINEL="$ST_SENTINEL" \
    bash "$0" self-test-volume-cache-probe >"$ST_WORK/e1.log" 2>&1
  ST_E1_RC=$?
  CARGO_TARGET_CACHE=/dev/null/ctd-review-target ST_PROBE_SENTINEL="" \
    bash "$0" self-test-volume-cache-probe >"$ST_WORK/e2.log" 2>&1
  ST_E2_RC=$?
  set -e
  rm -rf "$ST_SENTINEL_ROOT"
  if [ "$ST_E1_RC" != 0 ]; then echo "FAIL E: hostile 可写 cache 探测失败 rc=${ST_E1_RC}"; ST_PASS=0; fi
  if [ "$ST_E2_RC" != 0 ]; then echo "FAIL E: hostile 不可写 cache 探测失败 rc=${ST_E2_RC}"; ST_PASS=0; fi

  if [ "$ST_PASS" = 1 ]; then
    echo "self-test-volume 通过：A(匿名 create/1 次匹配 rm) B(fail closed/零 rm) C/D(mutation 检测) E(cache 隔离)"
  else
    echo "FAIL: self-test-volume 未通过（详见上）"
  fi
  exit $((1 - ST_PASS))
fi

# 未知/无参数命令：usage + 非零退出。绝不落入生产 preflight（M2——错误拼写的
# 测试命令不得静默成功或报无关 unbound 变量）。
echo "FAIL: 用法 build-sidecar-volume-test.sh {self-test-volume|self-test-volume-cache-probe}"
exit 1
