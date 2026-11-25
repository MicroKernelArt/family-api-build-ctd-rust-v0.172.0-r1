//! 官方 client factory 适配（TLS-CTD-001 §10.6 / FR-005 / FR-010）。
//!
//! 所有 upstream client 都由官方组件构造：
//! - UA/originator 来自 `codex_login::default_client` 官方 generator；
//! - Cloudflare cookie store 来自 `codex_http_client`；
//! - 显式账号级 proxy route 来自 fork patch 的
//!   `build_reqwest_client_for_explicit_route`；
//! - auth 使用官方 `BearerAuthProvider`（Authorization + ChatGPT-Account-ID）。
//!
//! 本模块不修改任何 TLS/H2/WS 配置或依赖版本。

use std::sync::Arc;
use std::time::Duration;

use codex_api::Provider;
use codex_api::RetryConfig;
use codex_api::SharedAuthProvider;
use codex_client::ReqwestTransport;
use codex_http_client::ClientRouteClass;
use codex_http_client::OutboundProxyRoute;
use codex_http_client::build_reqwest_client_for_explicit_route;
use codex_http_client::with_chatgpt_cloudflare_cookie_store;
use codex_model_provider::BearerAuthProvider;
use http::HeaderMap;
use http::HeaderValue;
use http::header::Entry;

use crate::contract::ProxyKind;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::redaction::SecretString;
use crate::transport::client_pool::ClientPool;
use crate::transport::proxy::ProxyDigest;
use crate::transport::proxy::ResolvedProxyRoute;
use crate::transport::proxy::{self};

/// 官方 ChatGPT Codex 端点 base（`/backend-api/codex`）。
pub const CHATGPT_CODEX_BASE_URL: &str = codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
/// WHAM 端点 base（`/backend-api`）。
pub const CHATGPT_WHAM_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// 官方 retry 配置（与 `ModelProviderInfo::to_api_provider` 相同）。
fn official_retry() -> RetryConfig {
    RetryConfig {
        max_attempts: 4,
        base_delay: Duration::from_millis(200),
        retry_429: false,
        retry_5xx: true,
        retry_transport: true,
    }
}

/// 测试目标 override（TLS-CTD-001 §15.2）：仅在 `test-upstream` cargo
/// feature 下编译；production CI 必须证明该 feature 关闭。base URL 只
/// 覆盖 Provider 目标，不改变封闭 operation 的 path/method 映射。
fn test_upstream_base_url(default: &str, env_key: &str) -> String {
    let _ = env_key; // 非 test-upstream 构建时未使用
    #[cfg(feature = "test-upstream")]
    {
        if let Ok(url) = std::env::var(env_key)
            && !url.is_empty()
        {
            return url;
        }
    }
    #[allow(unreachable_code)]
    default.to_string()
}

/// 官方 ChatGPT provider（codex base）。
pub fn chatgpt_provider() -> Provider {
    Provider {
        name: "chatgpt".to_string(),
        base_url: test_upstream_base_url(CHATGPT_CODEX_BASE_URL, "CTD_TEST_UPSTREAM_BASE_URL"),
        query_params: None,
        headers: official_version_header(),
        retry: official_retry(),
        stream_idle_timeout: Duration::from_secs(300),
    }
}

/// v3（hookpoint=v3）专用 provider：与 [`chatgpt_provider`] 同 base/retry/
/// idle timeout，但 **headers 为空**。
///
/// ★ 必需：`EndpointSession::make_request` 用
/// `provider.build_request(...)` 起步，再 `req.headers.extend(extra_headers)`。
/// `HeaderMap::extend` 对**同名键是替换**（`http` 1.4.0 `header/map.rs`
/// `Extend<(Option<HeaderName>, T)>` 的 `Entry::Occupied` 分支 `e.insert(val)`），
/// 但对 **provider 独有、Go 未交付的头则原样保留**。若沿用
/// [`chatgpt_provider`]，provider 自带的 `version`（`CARGO_PKG_VERSION`）会在
/// Go 未交付 `version` 时被 ctd 自行补上——**v3 下 ctd 不得做任何 header
/// 决策**，那会变成 ctd 谎报自身构建版本。
///
/// 因此 v3 的头集**完全由 Go 决定**，provider 不得贡献任何默认头。
pub fn chatgpt_provider_bare() -> Provider {
    Provider {
        name: "chatgpt".to_string(),
        base_url: test_upstream_base_url(CHATGPT_CODEX_BASE_URL, "CTD_TEST_UPSTREAM_BASE_URL"),
        query_params: None,
        headers: HeaderMap::new(),
        retry: official_retry(),
        stream_idle_timeout: Duration::from_secs(300),
    }
}

/// 官方 WHAM provider（usage/quota）。
pub fn wham_provider() -> Provider {
    Provider {
        name: "chatgpt-wham".to_string(),
        base_url: test_upstream_base_url(CHATGPT_WHAM_BASE_URL, "CTD_TEST_WHAM_BASE_URL"),
        query_params: None,
        headers: official_version_header(),
        retry: official_retry(),
        stream_idle_timeout: Duration::from_secs(300),
    }
}

/// ctd 出站 `version` 与 UA 版本段的**单一来源**。
///
/// `version` 请求头（[`official_version_header`]）与 `user-agent` 的
/// 客户端段/尾后缀（[`canonical_user_agent`]）都必须读它，禁止各自写字面量
/// 或各自读 `env!`——否则「头说 0.159.2、UA 说 0.159.0」这类同源漂移会重现
/// （`docs/22 §6` 项 #4）。
pub fn codex_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 官方 `version` header（与 `create_openai_provider` 相同）。
fn official_version_header() -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = http::HeaderValue::from_str(codex_version()) {
        headers.insert("version", value);
    }
    headers
}

/// 解析后的账号级 route（带 digest）。
pub struct RouteHandle {
    pub route: ResolvedProxyRoute,
    pub digest: ProxyDigest,
}

/// 解析 X-CTD proxy 输入为显式官方 route + 不可逆 digest。
pub fn resolve_route_handle(
    kind: ProxyKind,
    proxy_url: Option<&SecretString>,
) -> Result<RouteHandle, LocalError> {
    let route = proxy::resolve_route(kind, proxy_url)?;
    let digest = match &route {
        ResolvedProxyRoute::Direct => ProxyDigest::new("direct"),
        ResolvedProxyRoute::Proxy { url } => proxy::proxy_digest(url.as_str()),
    };
    Ok(RouteHandle { route, digest })
}

/// 官方 route-aware client 构建（显式 route；direct 忽略环境代理）。
pub fn build_official_client(
    route: &OutboundProxyRoute,
    route_class: ClientRouteClass,
) -> Result<reqwest::Client, LocalError> {
    let builder = reqwest::Client::builder()
        // F-002：封闭 operation 拒绝跨 host 重定向（任何 3xx 原样返回，
        // 第二跳零 socket；redirect 语义由 Go/caller 显式处理）。
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(codex_login::default_client::default_headers());
    let builder = with_chatgpt_cloudflare_cookie_store(builder);
    build_reqwest_client_for_explicit_route(builder, route, route_class).map_err(|err| {
        LocalError::new(
            "client_build_failed",
            ErrorClass::Transport,
            ErrorPhase::Connect,
            true,
            err.to_string(),
        )
    })
}

/// 从池获取官方 client（账号/凭据/proxy 隔离）。
pub async fn pooled_official_client(
    pool: &ClientPool,
    account_key: &str,
    credential_revision: &str,
    handle: &RouteHandle,
    operation: Operation,
) -> Result<reqwest::Client, LocalError> {
    let official_route = proxy::to_official_route(&handle.route);
    let route_class = operation.route_class();
    let key = crate::transport::client_pool::ClientPoolKey::new(
        account_key,
        credential_revision,
        handle.digest.clone(),
        route_class_label(route_class),
    );
    pool.get_or_insert_with(key, || {
        build_official_client(&official_route, route_class).map_err(|e| e.message)
    })
    .await
    .map_err(|message| {
        LocalError::new(
            "client_build_failed",
            ErrorClass::Transport,
            ErrorPhase::Connect,
            true,
            message,
        )
    })
}

fn route_class_label(class: ClientRouteClass) -> &'static str {
    match class {
        ClientRouteClass::Api => "api",
        ClientRouteClass::WebSocket => "wss",
        ClientRouteClass::Auth => "auth",
        ClientRouteClass::Other => "other",
    }
}

/// 官方 Bearer auth（Authorization + ChatGPT-Account-ID）。
pub fn official_auth(ctx: &crate::contract::RequestContext) -> SharedAuthProvider {
    Arc::new(BearerAuthProvider {
        token: Some(ctx.access_token.as_str().to_string()),
        account_id: ctx
            .chatgpt_account_id
            .as_ref()
            .map(|id| id.as_str().to_string()),
        is_fedramp_account: false,
    })
}

/// v1 专用 auth 装饰器：在内层 auth 之后，按官方位置注入
/// `originator` / `user-agent`。
///
/// ★ 为什么不能当普通请求头注入：官方把这两个头交给 **`reqwest::Client`
/// 默认头**承载，reqwest 在 `execute_request` 里对默认头做
/// 「按序插入、**不覆盖**已存在的同名头」
/// （`reqwest-0.12.28/src/async_impl/client.rs:2591-2597`），因此它们落在
/// `authorization` / `chatgpt-account-id` **之后**、`host` / `content-length`
/// **之前**（官方 0.159.2 出站序列第 15/16 位）。v1 的承载通道把这两个值
/// 交给 ctd，若在 `extra_headers` 阶段注入就会落到 auth 之前，头序立刻偏离
/// 官方。这里复用官方**同一个注入点**复刻该位置，语义与 reqwest 默认头逐字
/// 一致：仅当目标头不存在时写入。
///
/// 作用域：只有 v1 responses 路径构造它（[`official_auth`] 本身不变），
/// v3 与其它 operation 的头集/头序不受影响。
pub struct TrailingHeaderAuth {
    inner: SharedAuthProvider,
    trailing: HeaderMap,
}

impl codex_api::AuthProvider for TrailingHeaderAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), codex_api::AuthError> {
        // 遥测（`codex_api::auth_header_telemetry`）必须看到与出站一致的 auth 头。
        self.inner.add_auth_headers(headers)
    }

    fn apply_auth(&self, request: codex_client::Request) -> codex_api::AuthProviderFuture<'_> {
        Box::pin(async move {
            let mut request = self.inner.apply_auth(request).await?;
            for (name, value) in self.trailing.iter() {
                if let Entry::Vacant(entry) = request.headers.entry(name) {
                    entry.insert(value.clone());
                }
            }
            Ok(request)
        })
    }
}

/// 内层官方 auth + 官方位置的尾段头（v1 responses 专用）。
pub fn official_auth_with_trailing_headers(
    ctx: &crate::contract::RequestContext,
    trailing: HeaderMap,
) -> SharedAuthProvider {
    Arc::new(TrailingHeaderAuth {
        inner: official_auth(ctx),
        trailing,
    })
}

/// 官方 UA 形态（`docs/21 §3.1` 第 16 行）：
/// `<client>/<ver> (<OS> <osver>; <arch>) <TERM-program>/<ver>[ (<client>; <ver>)]`
///
/// 两条官方规则被复刻：
/// 1. **版本段一律取 [`codex_version`]**（与 `version` 请求头同源）——
///    承载通道带来的是客户端自报版本（如 `0.159.0`），与 `version` 头不同源；
/// 2. **尾后缀 `(<client>; <ver>)` 的 client 段 == `originator`**。官方后缀
///    由 `USER_AGENT_SUFFIX` 产生，来源是客户端自报名
///    （`app-server/src/request_processors/initialize_processor.rs:130`
///    `format!("{name}; {version}")`），且**仅当该名不在
///    `NON_ORIGINATING_CLIENT_NAMES`（`codex_app_server_daemon` / `codex-backend`）
///    内**才设置；此时 `originator` 也等于该名。可观测等价判据：
///    `originator == DEFAULT_ORIGINATOR`（`codex_cli_rs`，即「无客户端自报名」）
///    ⇒ 无尾后缀；否则有。该判据同时复现 exec / TUI / app-server 三形态实测
///    （`docs/21 §0.0`：exec 有、TUI 有、app-server 无）。
///
/// 平台段与终端段**保持承载值原样**——官方口径下它们随环境变（`docs/21 §7.2`），
/// 不是可比性缺陷。
///
/// `raw` 缺失时退回官方 generator [`codex_login::default_client::get_codex_user_agent`]，
/// 使「承载了 originator 但没承载 UA」时客户端段与后缀仍与 originator 自洽。
pub fn canonical_user_agent(raw: Option<HeaderValue>, originator: Option<&str>) -> String {
    let version = codex_version();
    let base = match raw.as_ref().and_then(|value| value.to_str().ok()) {
        Some(raw) => raw.to_string(),
        None => codex_login::default_client::get_codex_user_agent(),
    };
    let base = strip_user_agent_suffix(&base);
    let ua_client = base.split('/').next().unwrap_or(base).trim().to_string();
    let client = originator
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(ua_client.as_str());
    // 版本段之后的原文（平台段 + 终端段）；缺失则整段省略。
    let rest = base
        .split_once('/')
        .and_then(|(_, tail)| tail.split_once(' '))
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    let suffix = if client == codex_login::default_client::DEFAULT_ORIGINATOR {
        String::new()
    } else {
        format!(" ({client}; {version})")
    };
    if rest.is_empty() {
        format!("{client}/{version}{suffix}")
    } else {
        format!("{client}/{version} {rest}{suffix}")
    }
}

/// 去掉官方形态的尾后缀 `(<client>; <ver>)`（幂等：承载值已带后缀时不重复叠加）。
///
/// 只认「最后一个空格分隔组以 `(` 开头、以 `)` 结尾、内容含 `; `」**且不是**
/// 紧跟版本段的 `(<OS> <osver>; <arch>)` 组——后者正是平台段，绝不能剥。
fn strip_user_agent_suffix(user_agent: &str) -> &str {
    let user_agent = user_agent.trim();
    let Some(first_space) = user_agent.find(' ') else {
        return user_agent;
    };
    let Some(open) = user_agent.rfind(" (") else {
        return user_agent;
    };
    if open == first_space || !user_agent.ends_with(')') || !user_agent[open + 1..].contains("; ") {
        return user_agent;
    }
    user_agent[..open].trim_end()
}

/// ReqwestTransport 包装（复用池化 client）。
pub fn transport_for(client: reqwest::Client) -> ReqwestTransport {
    ReqwestTransport::new(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_provider_uses_official_base_and_retry() {
        let provider = chatgpt_provider();
        assert_eq!(provider.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(provider.retry.max_attempts, 4);
        assert!(provider.retry.retry_5xx);
        assert!(!provider.retry.retry_429);
    }

    #[test]
    fn wham_provider_uses_backend_api_root() {
        let provider = wham_provider();
        assert_eq!(provider.base_url, "https://chatgpt.com/backend-api");
    }

    #[test]
    fn bare_provider_contributes_no_default_headers() {
        // ★ v3（hookpoint=v3）必需：EndpointSession::make_request 用
        // provider.build_request 起步后 headers.extend(extra_headers)。
        // extend 对同名键是**替换**，但 provider 独有、Go 未交付的头会原样
        // 保留 ⇒ 非 bare provider 会让 ctd 在 v3 下自行补 version（谎报自身
        // 构建版本）。bare provider 保证 v3 的头集完全由 Go 决定。
        let bare = chatgpt_provider_bare();
        assert!(bare.headers.is_empty(), "bare provider 不得携带任何默认头");
        // 除默认头外与官方 provider 逐字段一致（同一 base/retry/idle）。
        let official = chatgpt_provider();
        assert_eq!(bare.base_url, official.base_url);
        assert_eq!(bare.name, official.name);
        assert_eq!(bare.retry.max_attempts, official.retry.max_attempts);
        assert_eq!(bare.stream_idle_timeout, official.stream_idle_timeout);
        // 官方 provider 恰好贡献 version（这正是 v3 必须绕开的那个头）。
        assert_eq!(official.headers.len(), 1);
        assert!(official.headers.contains_key("version"));
    }

    #[test]
    fn v3_header_set_is_go_owned_not_provider_augmented() {
        // 复现"若不换 bare provider 会发生什么"：Go 未交付 version 时，
        // 官方 provider 会替 ctd 补一个 CARGO_PKG_VERSION。
        let go_headers_without_version = http::HeaderMap::new();

        let mut with_official = chatgpt_provider().headers.clone();
        with_official.extend(go_headers_without_version.clone());
        assert_eq!(
            with_official.get("version").unwrap(),
            env!("CARGO_PKG_VERSION"),
            "非 bare provider 会在 Go 未交付时自补 version"
        );

        let mut with_bare = chatgpt_provider_bare().headers.clone();
        with_bare.extend(go_headers_without_version);
        assert!(with_bare.is_empty(), "bare provider 下头集完全由 Go 决定");

        // 同名键的语义是**替换**（不是 append）：Go 交付的 version 覆盖 provider 的。
        let mut go_headers = http::HeaderMap::new();
        go_headers.insert("version", http::HeaderValue::from_static("0.159.2"));
        let mut merged = chatgpt_provider().headers.clone();
        merged.extend(go_headers.clone());
        assert_eq!(merged.get_all("version").iter().count(), 1);
        assert_eq!(merged.get("version").unwrap(), "0.159.2");

        let mut merged = chatgpt_provider_bare().headers.clone();
        merged.extend(go_headers);
        assert_eq!(merged.get("version").unwrap(), "0.159.2");
    }

    /// `version` 请求头的实际出站值——UA 版本段必须与它同源。
    fn outbound_version_header() -> String {
        official_version_header()
            .get("version")
            .expect("version header")
            .to_str()
            .expect("ascii")
            .to_string()
    }

    #[test]
    fn user_agent_takes_official_tui_shape() {
        // ⑥ 参照官方 TUI 形态（baseline `reference_paths.secondary`）：承载值
        // 的客户端段版本（0.159.0）与尾后缀都要归一，平台/终端段保持环境原样。
        let version = outbound_version_header();
        let carried =
            HeaderValue::from_static("codex-tui/0.159.0 (Ubuntu 22.4.0; x86_64) xterm-256color");
        assert_eq!(
            canonical_user_agent(Some(carried), Some("codex-tui")),
            format!(
                "codex-tui/{version} (Ubuntu 22.4.0; x86_64) xterm-256color (codex-tui; {version})"
            )
        );
    }

    #[test]
    fn user_agent_client_segment_and_suffix_share_one_source() {
        // 客户端段版本与尾后缀版本必须同源于 `version` 请求头——这正是
        // docs/22 §6 项 #4「版本段 0.159.0 vs 官方 0.159.2」的成因。
        let version = outbound_version_header();
        let carried =
            HeaderValue::from_static("codex_exec/0.150.0 (Mac OS 15.5.0; arm64) WarpTerminal/v1");
        let user_agent = canonical_user_agent(Some(carried), Some("codex_exec"));
        assert!(
            user_agent.starts_with(&format!("codex_exec/{version} ")),
            "客户端段版本未与 version 头同源: {user_agent}"
        );
        assert!(
            user_agent.ends_with(&format!("(codex_exec; {version})")),
            "尾后缀版本未与 version 头同源: {user_agent}"
        );
    }

    #[test]
    fn user_agent_omits_suffix_for_default_originator() {
        // 官方 app-server 形态（clientInfo.name 命中 NON_ORIGINATING_CLIENT_NAMES）
        // 的 originator 保持默认值 ⇒ 官方 UA **无**尾后缀（docs/21 §0.0）。
        let carried =
            HeaderValue::from_static("codex_cli_rs/0.150.0 (Mac OS 15.5.0; arm64) WarpTerminal/v1");
        assert_eq!(
            canonical_user_agent(Some(carried), Some("codex_cli_rs")),
            format!(
                "codex_cli_rs/{} (Mac OS 15.5.0; arm64) WarpTerminal/v1",
                outbound_version_header()
            )
        );
    }

    #[test]
    fn user_agent_suffix_strip_is_idempotent() {
        // 承载值已带官方尾后缀时不重复叠加；平台段（紧跟版本段的括号组）绝不能被剥。
        let already = HeaderValue::from_static(
            "codex_exec/0.150.0 (Mac OS 15.5.0; arm64) WarpTerminal/v1 (codex_exec; 0.150.0)",
        );
        let once = canonical_user_agent(Some(already), Some("codex_exec"));
        let twice = canonical_user_agent(
            Some(HeaderValue::from_str(&once).expect("valid header")),
            Some("codex_exec"),
        );
        assert_eq!(once, twice);
        assert!(
            once.contains("(Mac OS 15.5.0; arm64)"),
            "平台段被误剥: {once}"
        );
        assert_eq!(once.matches(" (codex_exec; ").count(), 1);
    }

    #[test]
    fn version_header_comes_from_package_version() {
        let headers = official_version_header();
        assert_eq!(headers.get("version").unwrap(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn route_digest_is_stable_and_reversible_never() {
        let handle = resolve_route_handle(
            ProxyKind::Http,
            Some(&SecretString::from("http://user:pass@p:8080")),
        )
        .expect("route");
        assert_eq!(
            handle.digest.as_str(),
            proxy::proxy_digest("http://user:pass@p:8080").as_str()
        );
        assert_ne!(handle.digest.as_str(), "http://user:pass@p:8080");
    }

    #[test]
    fn direct_route_digest_is_marker() {
        let handle = resolve_route_handle(ProxyKind::Direct, None).expect("direct");
        assert_eq!(handle.digest.as_str(), "direct");
    }
}
