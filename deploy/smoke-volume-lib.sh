#!/usr/bin/env bash
# smoke volume ownership 生产函数库（R18-F2/FXF-0001）。
# 由 build-sidecar.sh 与 build-sidecar-volume-test.sh 共同 source（单一事实源）。
# ownership 仅在 create 成功后置 1；release/cleanup 只删 owned 资源。

# ---------- smoke volume ownership（R18-F2/FXF-0001） ----------
# build 流程与 self-test-volume 共用同一获取/释放逻辑（单一事实源）。
# ownership 仅在 create 成功后置 1；release 只删 owned 资源。
SMOKE_VOL=""
SMOKE_VOL_OWNED=0

acquire_smoke_volume() {
  if ! SMOKE_VOL="$(docker volume create)"; then
    echo "FAIL: smoke volume create 失败——fail closed"
    return 1
  fi
  SMOKE_VOL_OWNED=1
}

release_smoke_volume() {
  if [ "$SMOKE_VOL_OWNED" = 1 ]; then
    docker volume rm -f "$SMOKE_VOL" >/dev/null 2>&1 || true
  fi
}

# 生产清理函数（build 共享段 trap 与 self-test 共用同一函数，单一事实源）：
# 全部资源按 owned 标记清理。self-test 在子 shell 内先初始化引用变量再
# trap cleanup，与生产共享段完全一致。变异（删除 release 委托/重置 ownership
# 等）会被 self-test 击穿。
cleanup() {
  [ -n "$CID" ] && docker rm -f "$CID" >/dev/null 2>&1 || true
  [ "$ELF_CHECK_OWNED" = 1 ] && docker rm -f "$ELF_CHECK_NAME" >/dev/null 2>&1 || true
  release_smoke_volume
  rm -rf "$WORK_DIR" 2>/dev/null || true      # 独占目录整树删除（R10-F10）
  [ "$TARGET_OWNED" = 1 ] && rm -rf "$TARGET_DIR" 2>/dev/null || true
  [ -n "$CTX" ] && rm -rf "$CTX" 2>/dev/null || true
}

# 生产 callsite 完整序列（build 共享段与 self-test 共用同一函数）：
# 重置 ownership → acquire（成功后置 owned=1）。任何后续失败由调用方
# trap（cleanup → release_smoke_volume）清理。callsite 变异（named create/
# 忽略错误/ownership 重置）会被 self-test 场景 A/B 击穿。
smoke_volume_setup() {
  SMOKE_VOL=""
  SMOKE_VOL_OWNED=0
  acquire_smoke_volume
}

# 生产 callsite 封装（与共享段逐字一致）：setup 失败即 fail closed。
# "setup 后重置 ownership"类变异若插入此处会被 self-test 场景 A 击穿。
smoke_volume_callsite() {
  if ! smoke_volume_setup; then
    return 1
  fi
}
