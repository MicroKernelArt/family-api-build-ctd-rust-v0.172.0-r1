# UPSTREAM_UPGRADE（T-013 / TLS-CTD-001 FR-028）

每次官方 Codex 版本升级必须执行本 checklist；禁止只改版本字符串/expected
配置。升级产物（evidence bundle）是 release 门禁必需输入。

## Checklist

1. **重固定输入**
   - [ ] `git ls-remote https://github.com/openai/codex 'refs/tags/rust-v<新>^{}'` 记录 peeled commit
   - [ ] 校验新 tag 的 `codex-rs/Cargo.lock` SHA-256 与 toolchain（`rust-toolchain.toml`）
   - [ ] 更新 `UPSTREAM_BASE.md`（tag/commit/lock/toolchain/license）
2. **Clean rebase patch series**
   - [ ] 从新 tag 建 clean fork 分支，逐项重放 `PATCHES.md`
   - [ ] 重新审查 metadata/retry/proxy/WS 官方变更；新增/删除 patch 逐项映射需求
   - [ ] 更新 contract fixtures（`feature/tls/contract/v1/`）
3. **构建与证据**
   - [ ] `cargo build --locked --release -p codex-transportd`（1.95.0 或新固定 toolchain）
   - [ ] 更新 `/info` 的 expected 值（Go config `expected_codex_tag/commit/lock`）
   - [ ] 重新生成 SBOM / license report / image digest
4. **Wire 重测**
   - [ ] 重跑 `WIRE_COMPATIBILITY.md` 全矩阵（reference = 新 clean build）
   - [ ] 未解释差异为零；记录新 digest 对应 wire 证据
5. **Canary 重跑**
   - [ ] Go compatibility 全量测试
   - [ ] 单账号 canary + 停止红线（§21.2）+ 回滚演练
6. **发布**
   - [ ] evidence bundle（checklist + 测试输出 + wire capture + digest 链）归档
   - [ ] release note 引用该 bundle

## 禁止

- 只改 expected version 配置后直接上线；
- 在新版本内偷偷 backport 旧版本形态（RK-15）；
- 跳过 contract/wire 直接 promote image。
