# codex-transportd 运维 Runbook（T-011 / TLS-CTD-001 §20-21）

## 构建 profile

本地开发与发布是两条隔离链路。两个基础镜像都固定到同时包含
`linux/amd64` 和 `linux/arm64` 的 immutable OCI index；`prepare` 会显式按
profile 拉取目标平台，`build` 使用 `--pull=never` 和 `--network none`。

Apple Silicon 本地开发：

```bash
BUILD_PROFILE=local ./deploy/build-sidecar.sh prepare
BUILD_PROFILE=local ./deploy/build-sidecar.sh build --mode local-cargo
```

- 目标固定为 `linux/arm64` / `aarch64-unknown-linux-gnu`；Docker daemon 必须为 arm64。
- source identity 是命令启动时的 clean HEAD；构建期间 HEAD/tree 必须保持不变。
- image tag 使用 `:local` 命名空间，release id 必须以 `-dev` 结尾。
- receipt 写到 `~/.cache/codex-transportd/receipts/local-arm64.txt`，不会修改发布 receipt。
- Cargo target 默认复用 `~/.cache/codex-transportd/target/arm64`；可用
  `CARGO_TARGET_CACHE` 显式覆盖，脚本不会清理用户指定或 local profile 的持久 cache。

发布构建：

```bash
# BUILD_PROFILE=release 是默认值；发布机 Docker daemon 必须为 amd64。
export CTD_BUILD_RECEIPT_PATH=/var/lib/codex-transportd/evidence/build-receipt.txt
export CTD_RELEASE_RECEIPT_PATH=/var/lib/codex-transportd/evidence/release-receipt.txt
export CTD_RELEASE_ARTIFACT_PATH=/var/lib/codex-transportd/evidence/release-image.tar
export CTD_RELEASE_BINARY_PATH=/var/lib/codex-transportd/evidence/codex-transportd
export CTD_RELEASE_SBOM_PATH=/var/lib/codex-transportd/evidence/release.cdx.json
export CTD_RELEASE_LICENSE_REPORT_PATH=/var/lib/codex-transportd/evidence/license-report.json
./deploy/build-sidecar.sh prepare
CTD_RELEASE_RECEIPT_PATH="$CTD_BUILD_RECEIPT_PATH" \
./deploy/build-sidecar.sh build --mode local-cargo
bash patches/transportd/tools/materialize-release-artifact.sh
bash patches/transportd/tools/release-evidence-gate.sh --receipt "$CTD_RELEASE_RECEIPT_PATH" \
  --artifact "$CTD_RELEASE_ARTIFACT_PATH" --release
bash patches/transportd/tools/release-tag-verify.sh
```

- 目标固定为 `linux/amd64` / `x86_64-unknown-linux-gnu`。
- source identity 与 release id 来自 `patches/transportd/series.yaml`。
- build/release receipt、镜像 archive、镜像内 binary、SBOM 和 license report
  路径必须显式提供且位于 source tree 外；gate 会分别校验 source、artifact、
  binary、SBOM、review 与 release identity。
- local profile 的 image、release id 或 receipt 均不能作为发布证据。

## 部署

```bash
# 1. 先启动并验证 sidecar（mode=go 时不受影响，无 depends_on 阻塞）
docker compose up -d codex-transd
# 2. 手工检查 /info 与 build facts（dark deploy）
docker compose exec codex-transportd /usr/local/bin/codex-transportd healthcheck
# 3. 单账号灰度：目标账号 Extra 设置
#    openai_codex_transport_mode=rust_sidecar, openai_codex_protocol_mode=transportd
# 4. 扩量/全局切换：gateway.openai_codex_transport.mode=rust_sidecar
```

镜像必须使用 immutable digest（`CODEX_TRANSPORTD_DIGEST`）；任何 rebuild
先重跑 wire gate（`WIRE_COMPATIBILITY.md`）与 `UPSTREAM_UPGRADE.md`。

## 监控红线（§21.2 停止条件）

| 指标 | 红线 |
|---|---|
| `sub2api_codex_sidecar_fallback_total` | > 0 立即告警（正常必须为 0） |
| `sub2api_codex_go_egress_total`（完整模式） | > 0 立即告警 |
| sidecar local error 比例 | > 1% 持续 5 分钟 |
| version/capability mismatch | 任意一次立即告警 |
| p95 首语义 token 回归 | > 10% |
| RSS/FD/pool/queue | 持续增长 |
| compact success vs window advance | 长期不一致 |
| installation identity 变更 | 立即告警（VOLUME 意外丢失/替换） |

## 故障处置

- **readiness mismatch（tag/commit/lock/capability）**：停止新 sidecar 流量，
  检查 sidecar 镜像 digest 与 config 期望值；修正后重试。禁止静默回退 go。
- **UDS 权限问题**：两容器共享固定 GID（compose `group_add`/`user`），socket
  `0660`；非共享组无法连接属预期（VAL-006 双向测试）。
- **identity change**：确认 `codex_transport_home` volume 未被删除/替换；
  恢复备份或按“新安装实例”迁移流程处理。
- **容量拒绝**：`admission_rejected` 429/503 增长 → 检查 `CTD_MAX_INFLIGHT`/
  `CTD_MAX_WS_CONNECTIONS` 与 ion2 余量；调参后重测 PERF。
- **sidecar 崩溃/重启**：HTTP 自动恢复；存量 WS 以 1012 关闭由下游重连；
  installation ID / Redis session / DB 状态不丢失（named volume）。

## 回滚（显式，非请求内 fallback）

1. 停止新 sidecar 账号流量；
2. 目标账号 `openai_codex_transport_mode` 改回 `go`（或全局 mode 改回 go）；
3. 存量 sidecar WS 发送 1012/drain，等待下游重连（不热迁）；
4. 验证新请求只走 Go、golden/健康/计费正常；
5. 保留 sidecar image、日志、metrics、capture、installation volume 取证；
6. 若回滚 Sub2API 镜像：使用上线前精确 digest/config，不覆盖 PG/Redis 数据；
7. sidecar volume 不因普通回滚删除（除非明确废弃该安装实例）。

## 升级（每次官方版本）

执行 `UPSTREAM_UPGRADE.md` checklist，evidence bundle 归档为 release 必需输入。

## release 构建强制核对（P0-1/P0-2）

每次 release 构建后必须执行：

```bash
cargo build -p codex-transportd --release --no-default-features
# 启动 sidecar 后核对：
#   /info fork_commit == git rev-parse HEAD
#   /info fork_cargo_lock_sha256 == shasum -a 256 codex-rs/Cargo.lock
# 任一不匹配 → 构建不可发布（fork_commit 由 build.rs 在构建时 git 解析，
# .git/HEAD 变化会触发重跑；核对步兜底任何注入失效）
```
