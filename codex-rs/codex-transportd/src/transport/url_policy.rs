//! 下载 URL 校验策略(P1-C files / TLS-CTD-001 §16.4 / INV-006)。
//!
//! OP-020 file_content_fetch 的 download_url 来自上游响应(动态),必须
//! 在发出任何请求前校验:host ∈ 编译期官方 allowlist、path 前缀 ∈ 固定
//! 模板、无 query/fragment、无 path traversal。任何不匹配 → contract
//! error,零 DNS/socket I/O(SSRF 防护)。

use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;

/// 允许的下载 path 前缀(与官方端点模板一致)。
/// 允许的下载端点语法（M1/round-3）：明确模板，取代宽泛前缀。
/// - /backend-api/files/{id}/download
/// - /backend-api/conversation/{cid}/attachment/{aid}/download
///
/// 其余 path 一律拒绝（裸目录、任意深层路径）。
fn match_download_route(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["backend-api", "files", id, "download"] => is_id_segment(id),
        [
            "backend-api",
            "conversation",
            cid,
            "attachment",
            aid,
            "download",
        ] => is_id_segment(cid) && is_id_segment(aid),
        _ => false,
    }
}

fn is_id_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 128
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// e2e 追加 host(test-upstream 编译;逗号分隔,仅测试构建生效)。
#[cfg(feature = "test-upstream")]
fn test_extra_hosts() -> Vec<String> {
    std::env::var("CTD_TEST_ALLOWED_DOWNLOAD_HOSTS")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 官方 host 判定用 host；test 追加列表按 authority(host:port) 匹配，
/// 使 e2e 能区分同 host 不同端口的 fake（默认不包含非官方 host:port）。
fn host_allowed(host: &str, authority: &str) -> bool {
    if codex_http_client::is_allowed_chatgpt_host(host) {
        return true;
    }
    #[cfg(feature = "test-upstream")]
    {
        if test_extra_hosts().iter().any(|h| h == authority) {
            return true;
        }
    }
    #[cfg(not(feature = "test-upstream"))]
    {
        let _ = authority;
    }
    false
}

/// 校验上游提供的 download URL。
pub fn validate_download_url(raw: &str) -> Result<(), LocalError> {
    // normalize 前检查原始文本的 traversal(Url::parse 会先合并 ../ 段)。
    if raw
        .split('/')
        .any(|segment| segment == ".." || segment.contains(".."))
    {
        return Err(LocalError::new(
            "download_url_path_traversal",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL path traversal is rejected",
        ));
    }
    let parsed = url::Url::parse(raw).map_err(|_| {
        LocalError::new(
            "invalid_download_url",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL is not parseable",
        )
    })?;

    // scheme 仅 https(F-002：http 明文不允许出站)；test-upstream 的
    // loopback 测试端点放行 http（e2e 专用，生产构建无此分支）。
    let authority = parsed
        .host_str()
        .map(|h| {
            if let Some(port) = parsed.port() {
                format!("{h}:{port}")
            } else {
                h.to_string()
            }
        })
        .unwrap_or_default();
    let is_test_loopback = {
        #[cfg(feature = "test-upstream")]
        {
            test_extra_hosts().iter().any(|h| h == &authority)
        }
        #[cfg(not(feature = "test-upstream"))]
        {
            let _ = &authority;
            false
        }
    };
    if !is_test_loopback {
        if parsed.scheme() != "https" {
            return Err(LocalError::new(
                "invalid_download_url_scheme",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "download URL scheme must be https",
            ));
        }
        // effective port 必须为 443(F-002：非默认端口逃逸)。
        if parsed.port_or_known_default() != Some(443) {
            return Err(LocalError::new(
                "download_url_port_not_allowed",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "download URL port must be 443",
            ));
        }
    }
    // 禁 userinfo(F-002：credentials 不得嵌入 URL)。
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(LocalError::new(
            "download_url_userinfo_forbidden",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL must not carry userinfo",
        ));
    }

    // host 编译期官方 allowlist(复用官方 chatgpt host 判定)。
    let host = parsed.host_str().ok_or_else(|| {
        LocalError::new(
            "invalid_download_url_host",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL has no host",
        )
    })?;
    if !host_allowed(host, &authority) {
        return Err(LocalError::new(
            "download_url_host_not_allowed",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL host is not in the official allowlist",
        ));
    }

    // 无 query/fragment(保守:参数不得注入)。
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(LocalError::new(
            "download_url_query_forbidden",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL must not carry query or fragment",
        ));
    }

    // path 先严格 percent-decode 一次，再按明确端点语法匹配
    // （F-002/M1：%2e%2e、%2f、%5c、双重编码不得绕过；宽泛前缀拒绝）。
    let path = strict_percent_decode_once(parsed.path())?;
    if !match_download_route(&path) {
        return Err(LocalError::new(
            "download_url_path_not_allowed",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL path is not in the fixed template set",
        ));
    }
    if path
        .split('/')
        .any(|segment| segment == ".." || segment.contains(".."))
    {
        return Err(LocalError::new(
            "download_url_path_traversal",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL path traversal is rejected",
        ));
    }
    // 参数段字符集白名单(防编码注入；decode 后不允许 % 残留)。
    for segment in path.split('/').skip_while(|s| s.is_empty()) {
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(LocalError::new(
                "download_url_segment_charset",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "download URL segment contains forbidden characters",
            ));
        }
    }

    Ok(())
}

/// 严格 percent-decode 一次；非法编码、`/` `\` NUL、`%` 残留拒绝。
fn strict_percent_decode_once(raw: &str) -> Result<String, LocalError> {
    let mut out = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return Err(LocalError::new(
                    "download_url_invalid_encoding",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    "download URL path has invalid percent-encoding",
                ));
            }
            let hi = (bytes[i + 1] as char).to_digit(16).expect("hex");
            let lo = (bytes[i + 2] as char).to_digit(16).expect("hex");
            let decoded = (hi * 16 + lo) as u8;
            if matches!(decoded, b'/' | b'\\' | 0) || decoded == b'%' {
                return Err(LocalError::new(
                    "download_url_encoded_separator",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    "download URL path must not encode separators, NUL, or percent",
                ));
            }
            out.push(decoded);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| {
        LocalError::new(
            "download_url_encoding",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "download URL path is not valid UTF-8",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_download_urls_pass() {
        assert!(
            validate_download_url("https://chatgpt.com/backend-api/files/file_123/download")
                .is_ok()
        );
        assert!(
            validate_download_url(
                "https://chatgpt.com/backend-api/conversation/cv_1/attachment/att_2/download"
            )
            .is_ok()
        );
    }

    #[test]
    fn non_official_hosts_rejected() {
        let err = validate_download_url("https://evil.example/backend-api/files/x/download")
            .expect_err("evil host");
        assert_eq!(err.code, "download_url_host_not_allowed");
        let err =
            validate_download_url("https://chatgpt.com.evil.example/backend-api/files/x/download")
                .expect_err("suffix trick");
        assert_eq!(err.code, "download_url_host_not_allowed");
    }

    #[test]
    fn path_tampering_rejected() {
        let err = validate_download_url("https://chatgpt.com/backend-api/files/../../etc/passwd")
            .expect_err("traversal");
        assert_eq!(err.code, "download_url_path_traversal");
        let err = validate_download_url("https://chatgpt.com/not-a-download-path")
            .expect_err("path template");
        assert_eq!(err.code, "download_url_path_not_allowed");
        let err = validate_download_url("https://chatgpt.com/backend-api/files/x?token=abc")
            .expect_err("query");
        assert_eq!(err.code, "download_url_query_forbidden");
    }

    #[test]
    fn non_http_schemes_rejected() {
        let err = validate_download_url("file:///etc/passwd").expect_err("file scheme");
        assert_eq!(err.code, "invalid_download_url_scheme");
        // http 明文拒绝（F-002：仅 https）。
        let err = validate_download_url("http://chatgpt.com/backend-api/files/x/download")
            .expect_err("http scheme");
        assert_eq!(err.code, "invalid_download_url_scheme");
    }

    #[test]
    fn non_default_ports_rejected() {
        let err = validate_download_url("https://chatgpt.com:8443/backend-api/files/x/download")
            .expect_err("8443");
        assert_eq!(err.code, "download_url_port_not_allowed");
    }

    #[test]
    fn userinfo_rejected() {
        let err = validate_download_url("https://user@chatgpt.com/backend-api/files/x/download")
            .expect_err("userinfo");
        assert_eq!(err.code, "download_url_userinfo_forbidden");
    }

    #[test]
    fn broad_prefix_paths_rejected() {
        // M1：宽泛前缀下的裸目录/深层路径必须拒绝。
        for raw in [
            "https://chatgpt.com/backend-api/files/",
            "https://chatgpt.com/backend-api/files/file_1",
            "https://chatgpt.com/backend-api/files/file_1/download/extra",
            "https://chatgpt.com/backend-api/conversation/cv_1/attachment/",
            "https://chatgpt.com/backend-api/attachments/att_1/download",
            "https://chatgpt.com/backend-api/files/a/b/download",
        ] {
            let err = validate_download_url(raw).expect_err(raw);
            assert_eq!(err.code, "download_url_path_not_allowed", "{}", raw);
        }
    }

    #[test]
    fn encoded_separators_rejected() {
        for raw in [
            "https://chatgpt.com/backend-api/files/%2e%2e%2fetc/passwd/download",
            "https://chatgpt.com/backend-api/files/%252e%252e/etc/passwd/download",
            "https://chatgpt.com/backend-api/files/x%2fdownload",
            "https://chatgpt.com/backend-api/files/x%5cdownload",
            "https://chatgpt.com/backend-api/files/a%00b/download",
        ] {
            let err = validate_download_url(raw).expect_err(raw);
            assert!(
                err.code == "download_url_encoded_separator"
                    || err.code == "download_url_invalid_encoding"
                    || err.code == "download_url_path_not_allowed",
                "unexpected code {} for {}",
                err.code,
                raw
            );
        }
    }
}
