//! Secret 生命周期（TLS-CTD-001 §16.3 / FR-022）。
//!
//! token 与 proxy credential 进入 Rust 后立即封装为 [`SecretString`]；
//! Debug/日志/错误链一律输出 `<redacted>`。

use std::fmt;

/// 带 redaction 的 secret 字符串。
///
/// 内存中保存明文（必须用于 Authorization 等 header），但任何
/// `Debug`/`Display` 输出均为 `<redacted>`。不进入 tracing span field、
/// metrics label、error source chain 或 panic context。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretString {
    inner: String,
}

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            inner: value.into(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.inner
    }

    pub fn into_inner(self) -> String {
        self.inner
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// 代理 URL（可能含 credential）redaction：只保留 scheme/host，抹掉
/// userinfo 与 query。
pub fn redact_proxy_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "<invalid-url>".to_string();
    };
    let mut redacted = String::new();
    redacted.push_str(parsed.scheme());
    redacted.push_str("://");
    if let Some(host) = parsed.host_str() {
        redacted.push_str(host);
    }
    if let Some(port) = parsed.port() {
        redacted.push_str(&format!(":{port}"));
    }
    redacted
}

/// 将任意可能含 secret 的值包装为可记录的安全字符串。
pub fn redact(value: &str) -> &'static str {
    if value.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_never_debugs_plaintext() {
        let secret = SecretString::from("sk-super-secret-123");
        let debug = format!("{secret:?}");
        let display = format!("{secret}");
        assert_eq!(debug, "<redacted>");
        assert_eq!(display, "<redacted>");
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn proxy_url_redaction_keeps_host_drops_credentials() {
        let redacted = redact_proxy_url("http://user:pass@proxy.example.com:8080?token=abc");
        assert_eq!(redacted, "http://proxy.example.com:8080");
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("pass"));
        assert!(!redacted.contains("abc"));
    }

    #[test]
    fn invalid_proxy_url_redacts_to_marker() {
        assert_eq!(redact_proxy_url("::not-a-url::"), "<invalid-url>");
    }
}
