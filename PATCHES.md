# PATCHES（transportd/v0.163.0 相对 upstream rust-v0.163.0）

patches_source_commit: 851a0d173ba689c2066ed2d7f40698c46c514d4d
patches_source_tree: ab60fa3bcdadc0490526d60306e8fc24901769e8

本文件是 fork 的修改面与需求映射清单（TLS-CTD-001 §10.6 / FR-006）。下表每一行
都必须可映射到一个需求；P-row 只覆盖四类 adapter 改动：新增 binary crate、
metadata adapter、raw response adapter、显式 proxy route adapter。

P-row 表是需求映射的**子集**，不是完整修改面，也不构成「未列出文件与 upstream
一致」的声明。本线相对 upstream `rust-v0.163.0`
（`83086188ae0614bd882818ff1aa23f3194962012`）共修改 **246** 个路径：
`codex-rs/codex-transportd/` 内 63 个、`codex-rs/` 其它文件 25 个、非
`codex-rs/` 路径 158 个（`deploy/` 85、`patches/transportd/` 53、
`replay/` 4、`.github/workflows/` 4、`docs/` 2、`scripts/` 1、仓库根 9）。

> **基线沿革**：本线原基于 `rust-v0.159.0`（`687a119f0f`，206 个路径）。因 Go 侧
> sub2api 实现实际对齐 `rust-v0.159.2`，已整线重放到 `rust-v0.159.2`
> （`ff6aec96`）。0.159.0→0.159.2 的上游 delta 只有 75 个文件
> （+724/−183），与本线修改面仅在 `codex-rs/Cargo.toml` 与 `codex-rs/Cargo.lock`
> 两处相交，二者均已重解析/归一化。该次重放新增的 20 个路径全部落在
> `patches/transportd/full/`（17→23 个导出补丁）。
>
> **2026-10-02 二次重导出**：hookpoint v1/v3 对齐工作（`79de633..b525b18`，
> 8 个实现提交）折入系列，`source_commit` 由 `e517cf8` 前移到 `b525b183`
> （tree `db0a4bd1`）。导出补丁 23→**32**，修改面 226→**235** 个路径。
> 该批改动**不在**下方 P-row 表的 T-00x 需求映射内（P-row 是 CP-002 时代的
> 四类 adapter 映射子集），其需求与逐提交说明见 `up-1001/docs/04`。
>
> **2026-10-02 三次重导出**：安装标识账号隔离的 Rust 半步
> （`f337881`：`CARRIED_HEADER_NAMES` 增列 `x-codex-installation-id`，
> `effective_installation_id` 承载优先，v1 `/responses` 消费该头不再外发）
> 折入系列，`source_commit` 由 `b525b183` 前移到 `f337881f`
> （tree `6730446c`）。导出补丁 32→**34**，修改面 235→**244** 个路径。
> 同一提交把 `release_id` 由 `ctd-rust-v0.159.2-r1` 递增为
> `ctd-rust-v0.159.2-r2`：r1 已按 `b525b183` 构建并交付（内容不同的同 id
> 产物 = 身份碰撞），本次内容已变，必须换 id。
>
> **2026-10-17 整线重放到 `rust-v0.163.0`**（本线当前基线）。上游
> `rust-v0.159.2`→`rust-v0.163.0` 的 delta 是 3153 个文件
> （+166,426/−47,284），但与本线修改面只有 **8 个文件**相交：
> `codex-rs/Cargo.lock`、`codex-rs/Cargo.toml`、
> `codex-api/src/endpoint/responses_websocket.rs`（225 行，唯一真实代码冲突）、
> `codex-api/tests/clients.rs`、`login/src/auth/manager.rs`、
> `core/src/lib.rs`、`core/src/responses_metadata.rs`、
> `codex-api/src/endpoint/search.rs`。36 个提交逐条重放，只产生 **2 个冲突**：
> `Cargo.lock`（改为从上游官方锁重建，见 P-003）与
> `responses_websocket.rs` 的一个 import 块（两侧新增类型全保留）。
> 上游删除 `shell-escalation`、新增 `cloud-client` 两个 crate 对本线无影响
> （前者不是补丁目标，后者随 workspace 自然带入）。导出补丁 34→**37**
> （补入 r3 body-limit 提交与本次重钉提交），修改面 244→**246** 个路径。
> `release_id` 换代为 `ctd-rust-v0.163.0-r1`。

完整、逐行的机器生成清单见 `patches/transportd/modified-paths.txt`，生成命令
`bash patches/transportd/tools/gen-modification-surface.sh`（等价于
`git diff --name-only 83086188ae0614bd882818ff1aa23f3194962012 <source_commit>`）。
该清单中存在大量未被任何 P-row 命名的路径，例如整个 `deploy/vendor-debs*/`、
`patches/transportd/tools/`、`replay/`，以及 `codex-rs/` 下若干非 transportd 文件
（`codex-rs/codex-api/src/telemetry.rs`、`codex-rs/codex-client/src/{lib,retry}.rs`、
`codex-rs/codex-client/tests/retry_raw_tests.rs`、`codex-rs/core/src/lib.rs`、
`codex-rs/http-client/src/{transport,transport_tests}.rs`、
`codex-rs/scripts/check-platform-archives.sh`、
`codex-rs/websocket-client/src/cookie_tests.rs`）。因此本文件**不再**声明
「未列出文件一律与 upstream 一致」。

| # | 文件 | 变更 | 目的 | 需求 |
|---|---|---|---|---|
| P-001 | `codex-rs/codex-transportd/*` | 新增 crate（UDS server、health/info、installation bootstrap、封闭 operation、官方 adapter、client pool、WS relay） | sidecar 本体 | T-002/T-003/T-005/T-009 |
| P-002 | `codex-rs/Cargo.toml` | workspace members 增加 `codex-transportd` | 纳入 locked workspace | T-002 |
| P-003 | `codex-rs/Cargo.lock` | 以上游 `rust-v0.163.0` 官方锁为基线，增加 `codex-transportd` 包节点；本地包版本标签由 cargo 归一化 0.0.0→0.163.0（依赖图不变，官方 lock 本身在 cargo 1.95 下已 stale） | clean lockfile 构建（`--locked` 可复现） | T-002 |
| P-004 | `.github/workflows/transportd.yml` | CI locked build/test/clippy + artifact digest | build-proof CI | T-002 |
| P-005 | `UPSTREAM_BASE.md` / `PATCHES.md` / `codex-rs/codex-transportd/BUILD_PROOF.md` | fork 冻结证据 | CP-002 gate | T-002 |
| P-006 | `codex-rs/core/src/responses_metadata.rs` | 新增 `CodexResponsesMetadata::try_for_transport`（UUIDv7/window 校验窄构造器）与 `TransportMetadataError`；`client_metadata`/`compatibility_headers` 转 pub | metadata adapter（FR-009/FR-010） | T-005 |
| P-007 | `codex-rs/codex-api/src/endpoint/responses.rs` | 新增 `ResponsesClient::stream_raw`（复用 EndpointSession/auth/compression/retry，返回原始 status/headers/bytes stream） | raw response adapter（FR-011） | T-005 |
| P-008 | `codex-rs/codex-api/src/endpoint/responses.rs` + `codex-rs/codex-api/src/endpoint/session.rs` | 新增 `ResponsesClient::execute_raw_compact`（`POST /responses/compact` 的原始 status/headers/body；上游已删除专用 `CompactClient`，该路由在本线归 responses 客户端）与底层 `EndpointSession::execute_raw_with`（原始 unary 形态：terminal non-2xx 原样返回、携带官方 retry 后的真实 attempt 数） | raw response adapter（FR-012） | T-005/T-007 |
| P-009 | `codex-rs/codex-api/src/endpoint/models.rs` | 新增 `ModelsClient::execute_raw` | raw response adapter（FR-011） | T-008 |
| P-010 | `codex-rs/codex-api/src/endpoint/search.rs` | 新增 `SearchClient::execute_raw` | raw response adapter（FR-011） | T-008 |
| P-011 | `codex-rs/http-client/src/outbound_proxy.rs` + `lib.rs` | 新增 `build_reqwest_client_for_explicit_route`（Direct 强制 no_proxy；Proxy 精确 pin；失败不回退 direct） | 显式 proxy route adapter（FR-016/FR-017） | T-005 |
| P-012 | `codex-rs/websocket-client/src/lib.rs` | 新增 `WebSocketConnector::connect_with_route`（显式 route，不查系统/环境代理） | 显式 proxy route adapter（FR-016/FR-017） | T-005/T-009 |
| P-013 | `codex-rs/codex-api/src/endpoint/responses_websocket.rs` | 新增 `ResponsesWebsocketClient::connect_with_route`（官方 URL/headers/auth 构造 + 显式 route） | 显式 proxy route adapter（FR-016） | T-009 |
| P-014 | `codex-rs/login/src/auth/{personal_access_token,default_client,manager,mod}.rs` | 可见性 pub 化（`whoami_endpoint`、`PROD_AUTHAPI_BASE_URL`、`create_default_auth_client`、`refresh_token_endpoint` + re-export）；不改任何常量/语义 | 官方组件可见性最小开放：transportd auth 平面复用官方 auth 组件（P1-A/B） | T-012 |
| P-015 | `codex-rs/codex-api/src/endpoint/responses_websocket.rs` | 新增 `ResponsesWebsocketClient::connect_raw_with_route`（与 `connect_with_route` 相同 URL/header/auth/TLS/显式 route 构造，返回原始 `WebSocketConnection` + 握手响应，不包装 WsStream/telemetry） | raw response adapter（F-018：WS relay 复用官方连接逻辑，消除 sidecar 手工 URL/header/auth/connector 复制） | T-009 |

| P-016 | `deploy/build-sidecar.sh` | v8 release tag 由 `codex-rs/Cargo.lock` 的 resolved pin 推导（fail-closed：v8 包数必须恰为 1，否则报错退出）；构建容器内声明 `safe.directory`（否则 root 读 runner 克隆时被 dubious-ownership 拒绝 → build.rs 取 HEAD 失败 → `fork_commit` **静默留空**） | 让构建在 cold v8 cache 与 root 容器下都可复现、且身份不再静默丢失（patch 0017 内） | T-013 |

## 明确不改（禁止面）

- TLS cipher suites / extensions / ALPN / HTTP2 SETTINGS / WebSocket config；
- reqwest / rustls / tokio-tungstenite 依赖版本；
- 官方 retry 次数与 fallback 策略；
- 官方 UA / originator / metadata 常量；
- 不引入 TLS impersonation 库。

## 升级流程

任何 upstream 变更：先更新 `UPSTREAM_BASE.md` 重新固定 tag/commit/lock，
再 clean rebase 本 patch series，重跑 `UPSTREAM_UPGRADE.md` checklist 与
wire gate（T-013）。
