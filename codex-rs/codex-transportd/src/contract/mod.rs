//! 跨语言 contract v1（TLS-CTD-001 §11）。

pub mod error;
pub mod headers;
pub mod operation;
pub mod operations_snapshot;

pub use error::LocalError;
pub use headers::Hookpoint;
pub use headers::ProxyKind;
pub use headers::RequestContext;
pub use headers::RequestPurpose;
pub use headers::SessionSnapshot;

/// 原始上游响应（HTTP/SSE/compact）。
///
/// status/headers/body 均未解析重组；body 逐块流式回传。
pub struct RawUpstreamResponse {
    pub status: http::StatusCode,
    /// 过滤后的 upstream headers（hop-by-hop 已剔除）。
    pub headers: http::HeaderMap,
    /// 字节流（每个 chunk 到达即可被下游读取）。
    pub body: BodyStream,
    /// 低基数 attempt 计数（官方 retry 后的总尝试次数）。
    pub attempt_count: u32,
}

pub type BodyStream = futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>;

/// 过滤 hop-by-hop headers（§11.6）：Connection/Keep-Alive/Proxy-*/TE/Upgrade
/// 等只在单跳有意义；Set-Cookie 由官方 cookie store 消费，不透传下游。
pub fn filter_upstream_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    const HOP_BY_HOP: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "set-cookie",
        "proxy-connection",
    ];
    let mut out = http::HeaderMap::new();
    for (name, value) in headers {
        // F-016：上游不得注入内部 X-CTD-* 命名空间（origin/内部状态可被伪造）。
        // 先收集名称再删除，避免遍历时原地借用冲突。
        if HOP_BY_HOP.contains(&name.as_str()) || name.as_str().starts_with("x-ctd-") {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_x_ctd_namespace_from_upstream() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ctd-origin", "upstream-forged".parse().unwrap());
        headers.insert("x-ctd-request-id", "forged".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let filtered = filter_upstream_headers(&headers);
        assert!(
            filtered.get("x-ctd-origin").is_none(),
            "x-ctd-origin 必须剥离"
        );
        assert!(filtered.get("x-ctd-request-id").is_none());
        assert!(filtered.get("content-type").is_some());
    }

    #[test]
    fn filters_hop_by_hop_headers_only() {
        let mut headers = http::HeaderMap::new();
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("set-cookie", "a=b".parse().unwrap());
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("x-request-id", "req_upstream".parse().unwrap());
        let filtered = filter_upstream_headers(&headers);
        assert!(filtered.get("connection").is_none());
        assert!(filtered.get("transfer-encoding").is_none());
        assert!(filtered.get("set-cookie").is_none());
        assert_eq!(filtered.get("content-type").unwrap(), "text/event-stream");
        assert_eq!(filtered.get("x-request-id").unwrap(), "req_upstream");
    }

    #[test]
    fn preserves_duplicate_end_to_end_headers() {
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::WWW_AUTHENTICATE,
            "Bearer realm=\"primary\"".parse().unwrap(),
        );
        headers.append(
            http::header::WWW_AUTHENTICATE,
            "Basic realm=\"fallback\"".parse().unwrap(),
        );
        headers.insert(http::header::CONNECTION, "close".parse().unwrap());
        headers.insert(http::header::SET_COOKIE, "session=secret".parse().unwrap());
        headers.insert("x-ctd-origin", "forged".parse().unwrap());

        let filtered = filter_upstream_headers(&headers);
        let challenges = filtered
            .get_all(http::header::WWW_AUTHENTICATE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            challenges,
            vec!["Bearer realm=\"primary\"", "Basic realm=\"fallback\""]
        );
        assert!(filtered.get(http::header::CONNECTION).is_none());
        assert!(filtered.get(http::header::SET_COOKIE).is_none());
        assert!(filtered.get("x-ctd-origin").is_none());
    }
}
