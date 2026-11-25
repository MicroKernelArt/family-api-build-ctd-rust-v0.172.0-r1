# BUILD_PROOF（T-002 / CP-002）

> 本文件是 **build-proof**，不是最终 release candidate。HTTP candidate
> （T-005）与最终 release candidate（T-010）digest 另行记录。

## 冻结输入（当前基线：rust-v0.163.0）

- upstream commit：`83086188ae0614bd882818ff1aa23f3194962012`
- upstream `codex-rs/Cargo.lock` SHA-256：`17cdd862471774ba7aef836aaf0a0a3ed76bd23e503b1fc7278c89d1f2719d03`
- toolchain：`1.95.0`
- base image（production/reference 相同 digest，Q-02 冻结）：
  `debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258`
  （manifest list：`（CP-002 时代的 manifest list digest 已弃用，见 git 历史）`，
  采集命令见 `docs/build_proof_commands.md`）

## 构建命令

```bash
cd codex-rs
rustup toolchain install 1.95.0 --profile minimal --component clippy,rustfmt,rust-src
cargo build --locked -p codex-transportd
shasum -a 256 target/debug/codex-transportd
```

## Build-proof digest

<!-- CI/本地构建后回填 -->

```text
TBD（构建后回填 target/debug/codex-transportd SHA-256）
```
