# codex-transportd

Codex transport sidecar（T-002 / CP-002 骨架）。

## 职责

- UDS-only HTTP/1.1 server（Axum 0.8 + Tokio，official workspace 锁定版本）
- `GET /_internal/v1/health/live`、`GET /_internal/v1/health/ready`、
  `GET /_internal/v1/info`（契约路由由 `tests/contract_deployment.rs` 固定；
  旧文档中的 `/health/*` 路径不存在，任何请求外 path 一律 404）
- installation bootstrap：只通过官方 `codex_core::resolve_installation_id`
  在 `CODEX_HOME/installation_id` 完成（官方 resolver 是唯一写入者）
- `/info` 暴露 upstream/fork commit、lock SHA-256、toolchain、target、
  capabilities、base image digest 与 build-proof digest
- **无 TCP listener**（编译期 probe + 测试双重保证）

## 运行

```bash
cd ../..
cargo build --locked -p codex-transportd
cargo run -p codex-transportd -- --socket /run/ctd/sock/t.sock \
  --codex-home /var/lib/ctd/home \
  --socket-dir-uid 10001 --socket-dir-gid 2000 \
  --image-digest <sha256:...> --build-proof-digest <sha256:...>
curl --unix-socket /run/ctd/sock/t.sock http://localhost/_internal/v1/health/ready
curl --unix-socket /run/ctd/sock/t.sock http://localhost/_internal/v1/info
```

### CLI 契约（R1-codex，`tests/contract_deployment.rs` 固定）

| 参数 | 语义 |
|---|---|
| `--socket <path>` | 必填；UDS listener 路径（禁止 TCP） |
| `--codex-home <path>` | CODEX_HOME（缺省回退 `CODEX_HOME`/`HOME` 环境变量） |
| `--image-digest <digest>` | 运行期 `/info` image digest 覆盖 |
| `--build-proof-digest <digest>` | 运行期 `/info` build-proof digest 覆盖 |
| `--socket-dir-uid <uid>` | 期望 socket 目录属主 uid（部署准备） |
| `--socket-dir-gid <gid>` | 期望 socket 目录属主 gid（部署准备） |
| `--help` / `-h` | usage 后成功退出 |

未知参数、缺值、非法 uid/gid 立即失败（exit 2）。

### 跨仓 fixture 契约（R2-codex）

`tests/val006_adversarial.rs` 的 ops 契约清单测试默认以 `#[ignore]` 显式排除
（清洁拓扑由 `ops_fixture` 单元负向测试覆盖 fail-closed 行为）。三仓拓扑中
执行方式：

```bash
cd codex-rs   # cargo 需在 workspace 目录执行
export CTD_OPS_FIXTURE_MANIFEST="$PWD/codex-transportd/tests/fixtures/ops-integration-manifest.json"
export CTD_OPS_ROOT=/path/to/family-api-ops   # 期望 HEAD 见 manifest expected_commit
cargo test -p codex-transportd --test val006_adversarial -- --ignored
```

上下文缺失/身份不符一旦执行即失败（fail closed）；无任何 cwd/绝对路径回退。

### 所有权模型（R1-codex / PS-M4）

socket 目录与数据卷属主由**部署方准备**（如 `uid 10001`/`gid 2000`，
Compose/systemd init 或镜像层完成 chown）。daemon 启动时只做验证：

- 拒绝 symlink 与 world-writable 目录；
- 配置 `--socket-dir-uid/--socket-dir-gid` 时与目录属主精确比较；
- 绝不 chmod/chown/delete 任何部署或调用方资源；
- 已有活动 socket（连接成功）报 already running 拒绝启动；
- 仅 ECONNREFUSED 的 stale socket 才允许 unlink。

## 证据

- `BUILD_PROOF.md`：build-proof digest 与可复现命令
- `tests/lifecycle.rs`：installation 生命周期矩阵（重启/重建/新卷/备份恢复/注入）
- `scripts/installation_lifecycle.sh`：端到端二进制生命周期矩阵
