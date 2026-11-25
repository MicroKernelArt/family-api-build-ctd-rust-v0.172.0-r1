# replay0905 review squash map（7 groups ↔ 133 orig commits）

> **历史记录 / 已被取代（superseded）**：本文件是 2026-09-06 的 `replay0905` review
> 压缩记录，基线 `042fb41b`（rust-v0.153.4），其中「Phase C 0.153.4 API drift」等内容
> 属于该基线。本线已由 `replay0905-v0159-r1` 取代，并再升基线至 rust-v0.159.2
> （peeled `ff6aec96`；上一代基线为 rust-v0.159.0 / peeled `687a119f0f`）；当前线的补丁、身份与证据以 `patches/transportd/series.yaml`、
> `replay/manifest.yaml` 与 `PATCHES.md` 为准，本文件仅保留作逐 patch 溯源。

> 2026-09-06。本分支 `replay0905` 是 **review 视图**：把 codex transportd 的 133 个
> replay commit（`replay0905-upgrade` 线）按功能/scope 压缩为 7 个 commit，**tree 与
> granular HEAD 逐字节一致**（`git diff replay0905-upgrade replay0905` 为空）。
> 133 链原样保留在 `replay0905-upgrade`（含原 series 身份），供逐 patch 溯源/审计。
> series（133 patches）仍属于 granular 线；若日后定本分支为 release 线，需从本分支
> 重导出 series（N 个 patch + series.yaml 更新）并重跑 export-gate。

## 分支身份
- base：`042fb41b`（rust-v0.153.4，peeled commit `3d2ee51`）
- HEAD：`01bbb9b4`（tree `fb2ca99c` == `replay0905-upgrade` HEAD `bff998c` tree）
- 提交数：7（+本 docs commit = 8）

## 7 组映射（orig 序号 = `git log --reverse 042fb41b..replay0905-upgrade` 顺序）

| # | commit | 覆盖（orig） | scope 摘要 |
|---|---|---|---|
| 1 | `38876fc` | 1-10 | codex-transportd crate + P1 平面（scaffold、operation contracts、auth/files/agent、P-014、e2e harness、clippy） |
| 2 | `93cc130` | 11-23 | F 系列运行时安全（auth endpoint pin、files URL policy、contract namespace、UDS 单实例、agent 校验、ws admission/lifecycle、body limit、pool、metrics、ws adapter 复用） |
| 3 | `35edceb` | 24-40 | build-id 基础设施 + transportd 细节（build.rs fork 身份注入、socket 父目录验证、WS close 保真、config initializer、clippy/fmt、round3/4 修复、SBOM/WIRE 同步） |
| 4 | `026643a` | 41-48 | 可复现 OCI 构建（受审 Dockerfile、多阶段、.dockerignore、bind mount、/info digest 拆分、gitignore receipt） |
| 5 | `24e6acb` | 49-89 | release gate 与供应链证据（R7-R18 重做、receipt 序列、deploy bash3.2/--network none/v8 预取/vendor/deb、mini(R18)、volume 编排） |
| 6 | `c134d03` | 90-117 | arm64 native + capture 子系统 + ops fixtures + 重放收口（native arm64 构建/缓存、capture tee/budget/byte cap、reserved-keys 单一事实源、raw non-2xx retry、retry metrics） |
| 7 | `01bbb9b4` | 118-133 | CI/worker/平台接线（cross-platform worker、stacked refs、target-dir/LD、windows exclude）+ 确定性测试 + Phase C 0.153.4 API drift 适配 |

## 如何 review
- 逐组看：`git show <commit>`；组间递进 = 真实累计 delta（每个 commit 的 tree 取
  自 granular 链对应边界 commit，语义是"该 scope 在该时点的完整状态"）。
- 需要逐 patch 溯源/复核某一行来自哪个原始 commit：切到 `replay0905-upgrade` 用
  `git log -L` / `git blame`。
- series/export-gate/LEAK 验证只看 `replay0905-upgrade`（series 身份归属）。

## 已知取舍
- 压缩后丢失"commit ↔ 旧 132 系列"的逐条映射（granular 分支保留，可回查）。
- receipt 证据的 head/tree 指向 granular HEAD；若 release 走本分支需刷新。
- 未 push；未跑 hosted；codex receipt（bff998c）仍归属 granular 线。
