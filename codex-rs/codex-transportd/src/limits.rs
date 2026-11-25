//! 资源上限（TLS-CTD-001 §6.3 / FR-021 / INV-013）。
//!
//! 所有 buffer/queue/pool/并发均有静态上限；禁止无界 channel。
//!
//! 例外：请求体上限 [`max_body_bytes()`] 支持运行时通过环境变量
//! [`MAX_BODY_BYTES_ENV`] 调整（默认 [`MAX_BODY_BYTES_DEFAULT`]），
//! 以便无需重新编译即可放宽限制。

/// 全局并发 in-flight 请求上限。
pub const MAX_INFLIGHT: usize = 64;

/// UDS accepted connection 上限（包含尚未完成请求头解析的连接）。
pub const MAX_UDS_CONNECTIONS: usize = 128;

/// WS 并发连接上限（1:1）。
pub const MAX_WS_CONNECTIONS: usize = 32;

/// 请求体上限默认值（8 MiB）。
///
/// 与 Go 侧（sub2api）的 `server.max_body_mb` 默认值对齐。
pub const MAX_BODY_BYTES_DEFAULT: usize = 8 * 1024 * 1024;

/// 请求体上限的环境变量名（单位：MiB）。
pub const MAX_BODY_BYTES_ENV: &str = "CODEX_TRANSPORTD_MAX_BODY_MB";

/// 解析请求体上限（纯函数，便于测试）。
///
/// - `None` / 空串 / 非法数字 / `0` → 回落 [`MAX_BODY_BYTES_DEFAULT`]
/// - 合法正整数 `N` → `N` MiB（饱和乘法，防溢出）
pub fn parse_max_body_bytes(raw: Option<&str>) -> usize {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&mb| mb > 0)
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(MAX_BODY_BYTES_DEFAULT)
}

/// 运行时的请求体上限（进程内缓存一次）。
///
/// 读取环境变量 [`MAX_BODY_BYTES_ENV`]；修改该变量需要重启进程才能生效。
static MAX_BODY_BYTES: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
    parse_max_body_bytes(std::env::var(MAX_BODY_BYTES_ENV).ok().as_deref())
});

/// 返回运行时的请求体上限（字节）。
pub fn max_body_bytes() -> usize {
    *MAX_BODY_BYTES
}

/// 请求 header 总字节上限。
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// 单请求 header 数量上限。
pub const MAX_HEADER_COUNT: usize = 64;

/// 账号级 client pool 最大条目数。
pub const CLIENT_POOL_MAX_ENTRIES: usize = 256;

/// 池条目 idle TTL。
pub const CLIENT_POOL_IDLE_TTL_SECONDS: u64 = 300;

/// UDS admission 等待上限。
pub const ADMISSION_WAIT_MS: u64 = 500;

/// UDS dial 超时。
pub const UDS_DIAL_TIMEOUT_MS: u64 = 250;

/// 单 chunk 泵送大小（背压粒度）。
pub const CHUNK_SIZE: usize = 16 * 1024;

/// WS 每方向 frame 队列容量。
pub const WS_FRAME_QUEUE_CAPACITY: usize = 32;

/// 指标 channel 容量（观测系统变慢不能阻塞热路径）。
pub const METRICS_CHANNEL_CAPACITY: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_static_and_positive() {
        // const 断言在编译期验证（避免 clippy constant assertion）。
        const _: () = assert!(MAX_INFLIGHT > 0);
        const _: () = assert!(MAX_UDS_CONNECTIONS > 0);
        const _: () = assert!(MAX_WS_CONNECTIONS > 0);
        const _: () = assert!(MAX_BODY_BYTES_DEFAULT > 0);
        const _: () = assert!(MAX_HEADER_BYTES > 0);
        const _: () = assert!(MAX_HEADER_COUNT > 0);
        const _: () = assert!(CLIENT_POOL_MAX_ENTRIES > 0);
        const _: () = assert!(CHUNK_SIZE > 0);
        const _: () = assert!(WS_FRAME_QUEUE_CAPACITY > 0);
    }

    #[test]
    fn parse_max_body_bytes_falls_back_to_default() {
        // 未设 / 空串 / 非法 / 0 都回落默认 8 MiB。
        assert_eq!(parse_max_body_bytes(None), MAX_BODY_BYTES_DEFAULT);
        assert_eq!(parse_max_body_bytes(Some("")), MAX_BODY_BYTES_DEFAULT);
        assert_eq!(parse_max_body_bytes(Some("   ")), MAX_BODY_BYTES_DEFAULT);
        assert_eq!(parse_max_body_bytes(Some("abc")), MAX_BODY_BYTES_DEFAULT);
        assert_eq!(parse_max_body_bytes(Some("-1")), MAX_BODY_BYTES_DEFAULT);
        assert_eq!(parse_max_body_bytes(Some("0")), MAX_BODY_BYTES_DEFAULT);
        assert_eq!(parse_max_body_bytes(Some("8.5")), MAX_BODY_BYTES_DEFAULT);
    }

    #[test]
    fn parse_max_body_bytes_honors_value() {
        assert_eq!(parse_max_body_bytes(Some("1")), 1024 * 1024);
        assert_eq!(parse_max_body_bytes(Some("16")), 16 * 1024 * 1024);
        assert_eq!(parse_max_body_bytes(Some(" 32 ")), 32 * 1024 * 1024);
    }

    #[test]
    fn parse_max_body_bytes_saturates_on_overflow() {
        // 极大值不得 panic，饱和到 usize::MAX。
        assert_eq!(
            parse_max_body_bytes(Some(&usize::MAX.to_string())),
            usize::MAX
        );
    }

    #[test]
    fn max_body_bytes_is_positive_and_cached() {
        let a = max_body_bytes();
        let b = max_body_bytes();
        assert!(a > 0);
        assert_eq!(a, b, "缓存后两次调用必须一致");
    }
}
