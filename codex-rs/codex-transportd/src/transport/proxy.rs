//! 账号级 proxy route（TLS-CTD-001 §14）。
//!
//! - direct 显式禁用环境代理（`.no_proxy()`）；
//! - HTTP/HTTPS 支持，credential 只进入 secret wrapper；
//! - SOCKS5/SOCKS5H 未实现 → `unsupported_proxy_kind`，绝不回退 direct；
//! - proxy digest 只保留不可逆摘要，原始带凭据 URL 不进入 Debug 输出。

use sha2::Digest;
use sha2::Sha256;

use crate::contract::ProxyKind;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::redaction::SecretString;

/// 解析并归一化后的代理路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedProxyRoute {
    Direct,
    /// `url` 为归一化（可能含 credential）的代理 URL，以 secret wrapper 持有。
    Proxy {
        url: SecretString,
    },
}

/// 不可逆 proxy digest（pool key 组成之一）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyDigest(String);

impl ProxyDigest {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 归一化代理 URL（保留 credential 供出站使用，抹掉 fragment 等噪音）。
pub fn normalize_proxy_url(kind: ProxyKind, url: &str) -> Result<String, LocalError> {
    let parsed = url::Url::parse(url).map_err(|_| {
        LocalError::new(
            "invalid_proxy_url",
            ErrorClass::Config,
            ErrorPhase::ResolveProxy,
            false,
            "proxy URL is not parseable",
        )
    })?;
    if parsed.host_str().is_none() {
        return Err(LocalError::new(
            "invalid_proxy_url",
            ErrorClass::Config,
            ErrorPhase::ResolveProxy,
            false,
            "proxy URL has no host",
        ));
    }
    let expected_scheme = match kind {
        ProxyKind::Http => "http",
        ProxyKind::Https => "https",
        ProxyKind::Socks5 | ProxyKind::Socks5h => "socks5",
        ProxyKind::Direct => {
            return Err(LocalError::new(
                "invalid_proxy_url",
                ErrorClass::Config,
                ErrorPhase::ResolveProxy,
                false,
                "direct route must not carry a proxy URL",
            ));
        }
    };
    if parsed.scheme() != expected_scheme {
        return Err(LocalError::new(
            "proxy_scheme_mismatch",
            ErrorClass::Config,
            ErrorPhase::ResolveProxy,
            false,
            "proxy URL scheme does not match X-CTD-Proxy-Kind",
        ));
    }
    let mut normalized = url.to_string();
    normalized.truncate(parsed.as_str().len());
    Ok(normalized)
}

/// 计算不可逆 proxy digest（凭据参与摘要，但摘要本身不可逆）。
pub fn proxy_digest(url: &str) -> ProxyDigest {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    ProxyDigest(format!("{:x}", hasher.finalize()))
}

/// 解析账号级路由（TLS-CTD-001 §14.4：SOCKS fail closed）。
pub fn resolve_route(
    kind: ProxyKind,
    proxy_url: Option<&SecretString>,
) -> Result<ResolvedProxyRoute, LocalError> {
    match kind {
        ProxyKind::Direct => {
            if proxy_url.is_some() {
                return Err(LocalError::new(
                    "invalid_proxy_url",
                    ErrorClass::Config,
                    ErrorPhase::ResolveProxy,
                    false,
                    "direct route must not carry a proxy URL",
                ));
            }
            Ok(ResolvedProxyRoute::Direct)
        }
        ProxyKind::Http | ProxyKind::Https => {
            let raw = proxy_url.ok_or_else(|| {
                LocalError::new(
                    "missing_proxy_url",
                    ErrorClass::Config,
                    ErrorPhase::ResolveProxy,
                    false,
                    "non-direct route requires a proxy URL",
                )
            })?;
            let normalized = normalize_proxy_url(kind, raw.as_str())?;
            Ok(ResolvedProxyRoute::Proxy {
                url: SecretString::from(normalized),
            })
        }
        ProxyKind::Socks5 | ProxyKind::Socks5h => Err(LocalError::new(
            "unsupported_proxy_kind",
            ErrorClass::Config,
            ErrorPhase::ResolveProxy,
            false,
            "SOCKS5/SOCKS5H is not supported; failing closed without direct fallback",
        )),
    }
}

/// 将解析后的路由映射为官方 client/connector 消费的显式 route。
pub fn to_official_route(route: &ResolvedProxyRoute) -> codex_http_client::OutboundProxyRoute {
    match route {
        ResolvedProxyRoute::Direct => codex_http_client::OutboundProxyRoute::Direct,
        ResolvedProxyRoute::Proxy { url } => codex_http_client::OutboundProxyRoute::Proxy {
            url: url.as_str().to_string(),
            no_proxy: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_never_carries_url() {
        let err = resolve_route(
            ProxyKind::Direct,
            Some(&SecretString::from("http://proxy:8080")),
        )
        .expect_err("direct with url must fail");
        assert_eq!(err.code, "invalid_proxy_url");
    }

    #[test]
    fn http_proxy_normalizes_and_keeps_credentials() {
        let route = resolve_route(
            ProxyKind::Http,
            Some(&SecretString::from(
                "http://user:pass@proxy.example.com:8080",
            )),
        )
        .expect("resolve");
        let ResolvedProxyRoute::Proxy { ref url } = route else {
            panic!("expected proxy");
        };
        assert_eq!(url.as_str(), "http://user:pass@proxy.example.com:8080");
        // Debug 输出必须 redact credential。
        let debug = format!("{route:?}");
        assert!(!debug.contains("user"));
        assert!(!debug.contains("pass"));
    }

    #[test]
    fn socks_fails_closed() {
        for kind in [ProxyKind::Socks5, ProxyKind::Socks5h] {
            let err = resolve_route(kind, Some(&SecretString::from("socks5://proxy:1080")))
                .expect_err("socks must fail closed");
            assert_eq!(err.code, "unsupported_proxy_kind");
        }
    }

    #[test]
    fn scheme_mismatch_is_rejected() {
        let err = resolve_route(
            ProxyKind::Https,
            Some(&SecretString::from("http://proxy:8080")),
        )
        .expect_err("scheme mismatch");
        assert_eq!(err.code, "proxy_scheme_mismatch");
    }

    #[test]
    fn digest_is_irreversible_and_stable() {
        let d1 = proxy_digest("http://user:pass@proxy.example.com:8080");
        let d2 = proxy_digest("http://user:pass@proxy.example.com:8080");
        assert_eq!(d1, d2);
        assert_ne!(d1.as_str(), "http://user:pass@proxy.example.com:8080");
        assert_eq!(d1.as_str().len(), 64);
    }
}
