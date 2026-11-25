//! 编译期/运行期固定的 upstream 与构建证据。
//!
//! T-002 (CP-002) 冻结：fork 仓库拓扑、upstream commit、lock hash、
//! toolchain、base image digest 与 build-proof digest。该文件只读，不改
//! 官方 TLS/H2/WS 配置或依赖版本。

/// Release ID（R4-B2：外部 manifest 引用的不可变发布标识；
/// 构建流水线可覆盖为正式 ID，缺省为 dev）。
pub const RELEASE_ID: &str = match option_env!("CODEX_TRANSPORTD_RELEASE_ID") {
    Some(value) => value,
    None => "ctd-rust-v0.170.0-dev",
};

/// Upstream tag（官方 exact-tag fork 基准）。
pub const UPSTREAM_TAG: &str = "rust-v0.170.0";

/// Upstream peeled commit（`git ls-remote refs/tags/rust-v0.170.0^{}`）。
/// 72f97c30 是 annotated tag 对象；peeled source commit 才是 29eb79e9……
pub const UPSTREAM_COMMIT: &str = "29eb79e9866b3262a777fdf8195c1b9e63684c91";

/// Upstream `codex-rs/Cargo.lock` SHA-256（fork 之前官方锁文件）。
pub const UPSTREAM_LOCK_SHA256: &str =
    "3bd770fcba9f1cb9c998bf496c5c9f3bca7908fc9d462d8c4b3a9fd805f49202";

/// Rust toolchain（`codex-rs/rust-toolchain.toml`）。
pub const TOOLCHAIN: &str = "1.95.0";

/// 实际运行镜像 base（R8-F4：与 Dockerfile 运行阶段一致——debian
/// trixie-slim，digest 固定；不再报告官方上游的 Ubuntu 引用）。
pub const BASE_IMAGE_REF: &str =
    "debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258";

/// 编译目标三元组（build.rs 注入）。
pub const TARGET: &str = env!("CODEX_TRANSPORTD_TARGET");

/// 编译期注入的 fork commit（构建脚本/CI 传入），缺省为空字符串。
pub const FORK_COMMIT_FROM_ENV: &str = match option_env!("CODEX_TRANSPORTD_FORK_COMMIT") {
    Some(value) => value,
    None => "",
};

/// 编译期注入的 fork Cargo.lock SHA-256。
pub const FORK_LOCK_FROM_ENV: &str = match option_env!("CODEX_TRANSPORTD_FORK_LOCK") {
    Some(value) => value,
    None => "",
};

/// 编译期注入的 build-proof binary digest。
pub const BUILD_PROOF_FROM_ENV: &str = match option_env!("CODEX_TRANSPORTD_BUILD_PROOF_DIGEST") {
    Some(value) => value,
    None => "",
};

/// /info 暴露的 capabilities 列表（INV-015：只宣告已实现并经本地 e2e
/// 验证的 operation；P1 oauth/files/realtime 未过独立 gate 不宣告）。
pub const CAPABILITIES: &[&str] = &[
    "uds",
    "http1",
    "health",
    "info",
    "installation_bootstrap",
    "responses_http",
    "responses_compact",
    "responses_ws_v2",
    "models",
    "alpha_search",
    "usage",
    "quota_credits",
    "quota_consume",
    "auth_whoami",
    "auth_refresh",
    "file_download",
    "attachment_download",
    "file_content_fetch",
    "agent_task_register",
    // hookpoint 开关（步骤 5）：v1 是默认实现（恒存在）；v3 表示本构建实现了
    // responses 族的 v3 路径。Go 侧 EnsureReady 据此对 hookpoint==v3 fail
    // closed，杜绝"Go 已发 v3、旧 ctd 按 v1 服务"的静默混合。
    "hookpoint_v1",
    "hookpoint_v3",
];

#[cfg(test)]
mod tests {
    use super::CAPABILITIES;

    #[test]
    fn hookpoint_capabilities_are_advertised() {
        // 步骤 5：Go 的 EnsureReady 对 hookpoint==v3 做 capability 门。
        // 若本构建不广告 hookpoint_v3，灰度会被永久挡住（fail closed）；
        // hookpoint_v1 恒存在（v1 是默认实现）。
        for required in ["hookpoint_v1", "hookpoint_v3"] {
            assert!(
                CAPABILITIES.contains(&required),
                "capabilities 必须广告 {required}"
            );
        }
    }

    #[test]
    fn capabilities_are_unique_and_lower_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for capability in CAPABILITIES {
            assert!(seen.insert(*capability), "capability 重复：{capability}");
            assert!(
                capability
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "capability 形状非法：{capability}"
            );
        }
    }
}
