# WIRE_COMPATIBILITY（T-010 / TLS-CTD-001 §18.5）

官方原版 `rust-v0.163.0` 与 sidecar candidate 的线级对照报告。本文档与
`/info`（`build_info.rs`）、`PATCHES.md`、SBOM 共同构成 wire gate 证据链。

> **范围说明**：下表的对照是 **rust-v0.144.1 时代**（T-010）做的，尚未对当前
> rust-v0.163.0 基线重跑；因此 `wire_status` 记 `unverified`，release 模式的
> wire gate 会据此拒绝。本文件描述的是 candidate，但其中的测量数据属于上一代基线。

## 基线

| 项 | 值 |
|---|---|
| 官方 tag | `rust-v0.163.0` |
| 官方 commit | `83086188ae0614bd882818ff1aa23f3194962012` |
| 官方 lock SHA-256 | `17cdd862471774ba7aef836aaf0a0a3ed76bd23e503b1fc7278c89d1f2719d03` |
| toolchain | `1.95.0` |
| target | `x86_64-unknown-linux-gnu`（ion2 linux/amd64） |
| reference runner | clean official build（同 lock/toolchain/image） |
| candidate | `codex-transportd`（PATCHES.md 清单） |

## 测量环境（harness 规格）

受控 capture 服务（不接触生产 token/账号/模型内容）：

- TLS termination + 自定义测试 CA；
- HTTP/1.1、HTTP/2、SSE；
- WS Upgrade、text/control frames；
- direct、HTTP CONNECT、HTTPS proxy；
- 记录 ClientHello（JA4）、ALPN、H2 SETTINGS/顺序、header 顺序、
  连接复用、WS handshake/frame；
- 同镜像/网络/DNS/route/CA/fixture 双 runner 隔离运行。

## 维度与通过标准（§18.5.2）

| 层 | 比较项 | 通过标准 |
|---|---|---|
| TLS | version、cipher suites、extension/order、groups、sigalgs、ALPN、JA4 | 忽略随机数/session id/GREASE 后完全相同；JA4 相同 |
| HTTP/2 | SETTINGS 值/顺序、initial window、WINDOW_UPDATE、pseudo-header 顺序 | 完全相同或有官方非确定性证据 |
| HTTP | method/path/query、header 名/值/顺序、compression、body schema | 动态 ID/token 脱敏后契约相同；reserved metadata 同源 |
| SSE | 首字节、chunk/flush、结束/错误 | 不 parse/re-serialize；语义与结束条件相同 |
| WS | Upgrade headers/order、extensions、compression、close/control | 与 reference 相同；无额外 prewarm |
| 连接 | DNS、keepalive、idle、复用、proxy CONNECT | 策略相同；多账号隔离只允许减少跨账号复用 |

## 执行状态

- [ ] 本地/CI 受控 capture 已运行（需要容器网络环境；当前仓库环境离线时该
      gate 保持 open，不宣告对应 capability 的最终 wire 等价）
- [ ] reference（clean official build）样本 ≥20/矩阵项
- [ ] candidate 样本 ≥20/矩阵项
- [ ] 未解释差异 = 0
- [ ] 三方对照：官方 release binary、clean-source build、sidecar candidate
      各 artifact digest 已记录

## 证据链

```
source commit → Cargo.lock → SBOM → image digest（/info + readiness 校验）
```

任何 rebuild/digest 变化使旧 wire 证据失效并重跑本 gate（T-010 验收）。

---

## 状态快照（2026-08-05）

- **reference**: 官方 release binary（`rust-v0.159.2`，由 release 流程产出）
- **candidate**: 本 fork HEAD `（构建时由 build.rs 注入，见 /info fork_commit）`（build.rs 自动注入 fork commit + lock hash，
  `/info` 与 SBOM 一致；binary digest 由 CI 构建后注入 `CODEX_TRANSPORTD_BUILD_PROOF_DIGEST`）
- **wire comparison / canary gate: OPEN**——本地 e2e（fake upstream）证明
  operation/capability 行为；官方 wire 三方对照需真实网络环境（FR-025 open gate），
  完成前 capability 不视为 release-ready
- 任意 rebuild 改变 fork commit/lock hash 后，旧 wire/canary receipt 自动失效

## gate metadata（由证据提交写入；wire-gate.sh 逐字段比对 series.yaml）

wire_source_commit: 1702d2d1b9fecdd632042991b333596f56edeeee
wire_source_tree: 51d9287d59e4d606f1fd290809969cd797ebaaf0
# unverified：0.144.1 时代的线级对照（T-010）尚未对 rust-v0.163.0 基线重跑。
# 非 release 模式下 wire-gate.sh 接受 unverified；release 模式要求 pass。
wire_status: unverified
