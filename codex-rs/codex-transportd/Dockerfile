# syntax=docker/dockerfile:1.7
# codex-transportd sidecar 受审镜像（R3-F3/R5-F2/R6-F3/F4/R7-F1/F7）：
# 容器内多阶段 Linux 构建——宿主机 cargo 产物是 Mach-O/不同 target，
# 直接 COPY 无法在 linux/arm64 daemon 运行。
# 唯一构建入口: deploy/build-sidecar.sh（--locked、目标架构、ELF 校验、
# /info pin 校验、digest receipt）。
# 基础镜像 digest 固定（R6-F3）:
#   rust:1.99.1-bookworm  = sha256:4715c3ad743951a3cea90e8b432f2a17e0ef5f3f3c3abf75108879e783b396f0
#   debian:trixie-slim    = sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258

# 阶段 1：容器内构建（build.rs 需读 git 元数据——R6-F4：只读 bind mount
# 提供 context（含 .git），不 COPY 进 layer；远程 builder/cache 导出不
# 暴露仓库配置。target/ 用 cache mount 保持可写；registry/git db 亦缓存）。
# R7-F1：workspace manifest 位于 /src/codex-rs/Cargo.toml——WORKDIR 必须是
# /src/codex-rs（在 /src 执行 cargo 必然失败）；产物 install 到 /out，
# 不依赖 cache mount 在 RUN 之间保留（cache mount 内容只对构建层可见）。
FROM rust:1.99.1-bookworm@sha256:4715c3ad743951a3cea90e8b432f2a17e0ef5f3f3c3abf75108879e783b396f0 AS build
WORKDIR /src/codex-rs
# R8-F1：RELEASE_ID 经 build-arg 注入（build.rs option_env! 编译期读取）。
# 缺省值刻意取 *-dev：不经 build-arg 直接构建时，产出的就是 dev 镜像；
# 正式构建由 profile 层强制传入 series.yaml 的 release_id（不一致即拒绝）。
ARG RELEASE_ID=ctd-rust-v0.172.0-dev
ENV CODEX_TRANSPORTD_RELEASE_ID=$RELEASE_ID
RUN --mount=type=bind,source=.,target=/src,readonly \
    --mount=type=cache,target=/src/codex-rs/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p codex-transportd \
    && install -Dm755 target/release/codex-transportd /out/codex-transportd

# 阶段 2：精简运行镜像（digest 固定；R7-F7：apt 使用固定 Debian snapshot
# 日期，apt-get install 包版本可复现——snapshot 仓库内容不变）。
FROM debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258
# R8-F6：APT 完全离线可复现——vendor .deb 闭包（deploy/vendor-debs.py 从
# 固定 Debian snapshot 下载 + SHA256 校验）COPY 进镜像 dpkg -i 安装；
# 运行镜像不再执行 apt-get update/install，无可变仓库解析。
# curl/ca-certificates + 全部传递依赖版本与 SHA256 见 deploy/vendor-debs/manifest.sha256。
COPY vendor-debs/ /debs/
# R9-F9：安装前强制 sha256sum -c（manifest 与 vendor-debs.py 同源生成）；
# 任何包缺失/损坏/新增未记录 → 镜像构建失败（fail closed）。
# R10-F8：只安装 manifest 列出的文件（单事务 dpkg -i——逐个安装会因
# 依赖顺序失败；*.deb 通配会装未记录包）。
RUN cd /debs && sha256sum -c manifest.sha256 && \
    dpkg -i $(awk '{print $2}' manifest.sha256) && \
    rm -rf /debs
RUN groupadd -g 2000 codex-transportd && useradd -u 10001 -g 2000 -M -s /usr/sbin/nologin codex-transportd
WORKDIR /app
COPY --from=build /out/codex-transportd /app/codex-transportd
RUN chmod 0755 /app/codex-transportd
USER 10001:2000
EXPOSE 8080
# 显式 ENTRYPOINT（R3-F3）：CLI 参数由 compose command 提供。
ENTRYPOINT ["/app/codex-transportd"]
