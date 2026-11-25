# 上游同步与补丁重放手册（UPSTREAM MERGE）

> 目标：两个上游（sub2api 官方、codex-rs 官方）升级时，最低成本对齐并保留
> 自身二开（业务层 52 commits + sidecar 层 15 commits）。
> 模式：upstream-first + 分层 patch-stack（业务层 merge、sidecar 层 rebase）。
> 前置：两仓 `rerere.enabled=true` 已开；Go `upstream` 分支跟踪 `origin/main`；
> Rust `upstream` remote = openai/codex。

---

## 0. 分层结构（Go 侧）

```
官方 origin/main（147 commits 领先 v0.1.169，持续演进）
   │  merge ①（业务层对齐）
   ▼
custom/fingerprint（52 个业务二开 commits = deploy-0805 基底；持续演进）
   │  rebase ②（sidecar 补丁栈重放）
   ▼
tls-finger-consolidated（8 功能补丁 = 全部 codex-transport 工作）
   │  重建 ③（部署层 3 commits 重放）
   ▼
v0.1.169-deploy-sidecar（部署）
```

Rust 侧同理：官方 `upstream`（codex-rs）→ patch 重放（P-014/015/016）→
`transportd/consolidated`（9 功能补丁）→ 重建部署二进制。

---

## 1. Go 侧升级流程

### 1.1 同步官方上游（merge ①）

```bash
cd /Users/shanks/Documents/my-ai-api/sub2api
git fetch origin
# 业务层对齐：官方 main → custom/fingerprint
git checkout custom/fingerprint
git merge origin/main          # 冲突：业务补丁 vs 官方新版（rerere 记忆后自动重放）
# 甄别 52 个业务 commits：官方新版已含同类修复 → 弃本地；私有区域冲突 → 手动解
git push origin custom/fingerprint   # 推送回业务线（如有权限）
```

### 1.2 sidecar 补丁栈 rebase（rebase ②）

```bash
# 把 8 个功能补丁从旧基底重放到业务层最新
git checkout tls-finger-consolidated
git rebase --onto custom/fingerprint <旧基底> tls-finger-consolidated
# 旧基底 = 本次升级前 tls-finger-consolidated 的 merge-base(custom/fingerprint)
# 冲突预期：仅 8 个接线点（sidecarDo 调用处）；rerere 自动重放
```

> rebase 时接线点冲突多为"插入点偏移"，rerere 首次记忆后自动解决。
> 若功能补丁依赖业务层新 API，按依赖顺序逐个 `git rebase --onto` 或交互式调整。

### 1.3 重建部署分支（重建 ③）

```bash
git checkout -b v0.1.170-deploy-sidecar tls-finger-consolidated
git cherry-pick <3 个部署 commits>   # 或重放 deploy 文件变更
# 更新 compose：EXPECTED_FORK_COMMIT = 新 sidecar /info 实测值（见 §3）
```

### 1.4 验证门禁（每次升级必跑）

```bash
go test ./internal/service/ ./internal/config/ ./internal/handler/
go test -race ./internal/service/ -run "TestCodexTransport|TestSidecar|TestTransportForAccount"
CODEX_TRANSPORTD_BIN=<重建 sidecar> go test -tags e2e ./internal/service/ -run TestE2E -v
# 身份：sidecar /info fork_commit == 当前 Rust HEAD（见 §3）
```

---

## 2. Rust 侧升级流程

### 2.1 同步官方 codex-rs

```bash
cd /Users/shanks/Documents/my-ai-api/codex-transportd
git fetch upstream                     # openai/codex 官方
git branch -f upstream-tracking upstream/main   # 记录官方最新（只读跟踪）
# 查看官方新 tag/commit：
git log --oneline 44918ea..upstream/main | head -20   # 评估升级范围
```

### 2.2 重放补丁（patch 重放）

```bash
# 方法 A（推荐）：新分支从官方最新开始，逐个 apply 补丁
git checkout -b transportd/upgrade-<新版本> upstream/main
git apply --check patches/transportd/P-014-login-visibility.patch && git apply patches/transportd/P-014-login-visibility.patch
git apply --check patches/transportd/P-015-codex-api-raw-adapters.patch && git apply patches/transportd/P-015-codex-api-raw-adapters.patch
git apply --check patches/transportd/P-016-transport-adapters.patch && git apply patches/transportd/P-016-transport-adapters.patch
# 冲突 → 手动修（补丁极小，通常仅 P-014 的 2-4 行 pub 化可能随官方重构移位）

# 方法 B（替代）：把 codex-transportd crate 目录整体拷入新官方树
cp -r codex-rs/codex-transportd <新树>/codex-rs/
# 然后按 Cargo.toml/Cargo.lock 冲突解决（workspace 成员 + lock 更新）
```

### 2.3 重建与门禁

```bash
cd codex-rs
cargo build -p codex-transportd --release          # 生产
cargo test -p codex-transportd && cargo test -p codex-transportd --features test-upstream
cargo clippy --locked -p codex-transportd --all-targets --all-features --no-deps -- -D warnings
cargo fmt --all -- --check
# 重跑 e2e（Go 侧，用新二进制）
# 更新 SBOM.md / WIRE_COMPATIBILITY.md / BUILD_PROOF.md（新 HEAD + lock hash）
```

### 2.4 官方补丁本身的更新（若官方已合入 P-014/015）

每次升级先检查 `git log 44918ea..upstream/main -- codex-rs/login codex-rs/codex-api`：
- 官方已采纳 → 删除对应 patch 文件（零成本对齐）；
- 未采纳但官方重构了相关文件 → 更新 patch（通常 5 分钟内）。

---

## 3. 部署侧（升级后必做）

```bash
# 1. 重建 sidecar 二进制 + 镜像（见 sub2api/deploy/SIDECAR_DEPLOY.md §5）
# 2. 读取新 /info 身份字段：
docker run --rm sub2api:vX.Y.Z-sidecar /app/codex-transportd --socket /tmp/s.sock --codex-home /tmp/h &
curl -s --unix-socket /tmp/s.sock http://localhost/_internal/v1/info
# 3. 填 compose 的 EXPECTED_*（release_id / fork_commit / fork_cargo_lock_sha256）
# 4. 切镜像 + 验证身份 pin：sidecar /info 与 Go 配置不一致 → secret 零出站（fail closed）
ssh ion2 'cd /opt/sub2api && ./switch.sh status'
```

---

## 4. 升级节奏与回滚

| 项 | 约定 |
|---|---|
| 频率 | 跟随上游 minor 更新；业务层 merge 可随时（低风险），sidecar 层 rebase 定期批量 |
| 回滚 | 每次升级一个独立 commit（可 revert）；镜像 tag 保留上一版；`switch.sh go` 回纯 Go |
| 补丁漂移 | 每轮升级后立即跑 e2e + 身份核对，漂移在当轮暴露 |

## 5. 成本预估（升级一次）

| 步骤 | 预计耗时 | 说明 |
|---|---|---|
| Go merge ①(业务层) | 0.5-2h | 52 业务补丁 vs 官方 147 commits;rerere 记忆后显著下降 |
| Go rebase ②(sidecar) | 15-30min | 8 接线点冲突,rerere 自动 |
| Rust 重放 + 门禁 | 1-2h | 3 个 patch 极小;主要成本是编译+测试 |
| 部署重建 + 验证 | 30min | 镜像 + EXPECTED_* 更新 + e2e |
| **合计** | **2-5h/轮** | 首次高(建立记忆),后续趋向 1-2h |
