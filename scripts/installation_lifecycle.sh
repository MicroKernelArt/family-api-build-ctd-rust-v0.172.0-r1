#!/usr/bin/env bash
set -euo pipefail

# installation 生命周期矩阵（CP-002）：进程重启 / 同 volume 重建 / 新空 volume
# / 备份恢复 / 注入尝试。使用官方 resolver 语义（经二进制 /health/ready 验证）。

BIN="${1:-codex-rs/target/debug/codex-transportd}"
if [ ! -x "$BIN" ]; then
  echo "usage: $0 <path-to-codex-transportd-binary>" >&2
  exit 2
fi

WORK="$(mktemp -d)"
cleanup() { kill "${SERVER_PID:-}" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

run_server() {
  local home="$1" socket="$2"
  "$BIN" --socket "$socket" --codex-home "$home" >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 50); do
    if curl -s --unix-socket "$socket" http://localhost/health/live >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "server failed to start: $(cat "$WORK/server.log")" >&2
  return 1
}

get_id() {
  curl -s --unix-socket "$1" http://localhost/health/ready \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["installation_id"])'
}

VOLUME="$WORK/volume"
mkdir -p "$VOLUME"

SOCK1="$WORK/s1.sock"
run_server "$VOLUME" "$SOCK1"
ID1="$(get_id "$SOCK1")"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
rm -f "$SOCK1"

# 进程重启：同一 volume 再次启动。
SOCK2="$WORK/s2.sock"
run_server "$VOLUME" "$SOCK2"
ID2="$(get_id "$SOCK2")"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
rm -f "$SOCK2"
[ "$ID1" = "$ID2" ] || { echo "FAIL: restart changed id"; exit 1; }

# 同 volume 容器重建 = 再次挂载同一目录启动。
SOCK3="$WORK/s3.sock"
run_server "$VOLUME" "$SOCK3"
ID3="$(get_id "$SOCK3")"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
rm -f "$SOCK3"
[ "$ID1" = "$ID3" ] || { echo "FAIL: same-volume rebuild changed id"; exit 1; }

# 新空 volume：新目录必须得到新 ID。
NEWVOL="$WORK/new-volume"
mkdir -p "$NEWVOL"
SOCK4="$WORK/s4.sock"
run_server "$NEWVOL" "$SOCK4"
ID4="$(get_id "$SOCK4")"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
rm -f "$SOCK4"
[ "$ID1" != "$ID4" ] || { echo "FAIL: new volume reused id"; exit 1; }

# 备份/恢复：删除后从备份恢复，ID 不变。
BACKUP="$WORK/installation_id.backup"
cp "$VOLUME/installation_id" "$BACKUP"
rm "$VOLUME/installation_id"
cp "$BACKUP" "$VOLUME/installation_id"
SOCK5="$WORK/s5.sock"
run_server "$VOLUME" "$SOCK5"
ID5="$(get_id "$SOCK5")"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
rm -f "$SOCK5"
[ "$ID1" = "$ID5" ] || { echo "FAIL: backup/restore changed id"; exit 1; }

# 注入尝试：非 UUID 内容必须被官方 resolver 拒绝并重写。
printf 'injected-not-a-uuid' > "$VOLUME/installation_id"
SOCK6="$WORK/s6.sock"
run_server "$VOLUME" "$SOCK6"
ID6="$(get_id "$SOCK6")"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
rm -f "$SOCK6"
PERSISTED="$(cat "$VOLUME/installation_id")"
[ "$PERSISTED" != "injected-not-a-uuid" ] || { echo "FAIL: injection accepted"; exit 1; }
echo "LIFECYCLE_MATRIX=pass id=$ID1 restart=$ID2 rebuild=$ID3 new_volume=$ID4 restore=$ID5 injection_rejected=$ID6"
