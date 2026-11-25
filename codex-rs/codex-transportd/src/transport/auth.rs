//! P1 auth 平面执行器(T-012 / TLS-CTD-001 §12.5)。
//!
//! 只执行封闭 auth operation(auth.openai.com 固定端点);token 不落盘
//! sidecar,凭据经 X-CTD secret 头单请求传入,新 token 由 Go 持久化并
//! 更新 credential revision。未通过独立 gate 的 operation 由 server 层
//! 返回 capability_not_released。

use bytes::Bytes;
use http::HeaderMap;
use http::StatusCode;

use crate::contract::RawUpstreamResponse;
use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::transport::client_pool::ClientPool;
use crate::transport::official_adapter;

/// 生产 OAuth refresh 端点：编译期常量，绝不读通用环境变量（F-001）。
const PROD_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";

/// 官方 auth API base(测试 override 仅 test-upstream feature 编译)。
pub(crate) fn auth_api_base_url() -> String {
    #[cfg(feature = "test-upstream")]
    {
        if let Ok(url) = std::env::var("CTD_TEST_AUTH_BASE_URL")
            && !url.is_empty()
        {
            return url;
        }
    }
    codex_login::auth::PROD_AUTHAPI_BASE_URL.to_string()
}

/// 官方 PAT whoami 原始请求(固定端点;Go 保留 401/403 语义判断)。
pub async fn execute_whoami_raw(
    pool: &ClientPool,
    ctx: &RequestContext,
) -> Result<RawUpstreamResponse, LocalError> {
    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::OAuthWhoami,
    )
    .await?;

    let url = codex_login::auth::whoami_endpoint(&auth_api_base_url());
    let response = client
        .get(&url)
        .bearer_auth(ctx.access_token.as_str())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|err| {
            LocalError::new(
                "auth_whoami_failed",
                ErrorClass::Transport,
                ErrorPhase::Connect,
                true,
                format!("whoami request failed: {err}"),
            )
        })?;

    raw_from_reqwest_response(response, Operation::OAuthWhoami).await
}

/// 官方 OAuth refresh(JSON 形态,官方组件语义)。
///
/// `refresh_token` 经 X-CTD-Refresh-Token secret 头传入;新 token 由 Go
/// 持久化(§12.5:caller 负责持久化,sidecar 不落盘)。
pub async fn execute_refresh_raw(
    pool: &ClientPool,
    ctx: &RequestContext,
) -> Result<RawUpstreamResponse, LocalError> {
    let refresh_token = ctx.refresh_token.as_ref().ok_or_else(|| {
        LocalError::new(
            "missing_refresh_token",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "X-CTD-Refresh-Token is required for oauth_refresh",
        )
    })?;

    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::OAuthRefresh,
    )
    .await?;

    let url = refresh_endpoint()?;
    // client_id 参数化：Sub2API 用自有 OAuth app 签发 refresh_token，
    // 不能使用官方 CLI client_id（否则服务端拒绝）。缺失时回退官方常量。
    let client_id = ctx
        .oauth_client_id
        .clone()
        .unwrap_or_else(codex_login::auth::oauth_client_id);
    let body = serde_json::json!({
        "client_id": client_id,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token.as_str(),
    });
    let response = client.post(&url).json(&body).send().await.map_err(|err| {
        LocalError::new(
            "auth_refresh_failed",
            ErrorClass::Transport,
            ErrorPhase::Connect,
            true,
            format!("refresh request failed: {err}"),
        )
    })?;

    raw_from_reqwest_response(response, Operation::OAuthRefresh).await
}

/// OAuth refresh 端点解析（F-001）：
/// - 生产：编译期常量 `PROD_REFRESH_URL`，任何环境变量都无法改变；
/// - test-upstream：`CTD_TEST_AUTH_REFRESH_URL` 仅允许 loopback，
///   非 loopback（含域名）在 DNS/socket 之前拒绝。
fn refresh_endpoint() -> Result<String, LocalError> {
    #[cfg(feature = "test-upstream")]
    {
        if let Some(raw) = std::env::var_os("CTD_TEST_AUTH_REFRESH_URL") {
            let raw = raw.to_string_lossy();
            let parsed = url::Url::parse(&raw).map_err(|err| {
                LocalError::new(
                    "invalid_test_auth_refresh_url",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    format!("CTD_TEST_AUTH_REFRESH_URL is not a valid URL: {err}"),
                )
            })?;
            let host = parsed.host_str().unwrap_or_default().to_string();
            let is_loopback = match parsed.scheme() {
                "http" | "https" => {
                    host == "127.0.0.1"
                        || host.starts_with("127.")
                        || host == "::1"
                        || host == "[::1]"
                }
                _ => false,
            };
            if !is_loopback {
                return Err(LocalError::new(
                    "test_auth_refresh_url_not_loopback",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    "CTD_TEST_AUTH_REFRESH_URL must be a loopback endpoint (no DNS)",
                ));
            }
            return Ok(parsed.to_string());
        }
    }
    Ok(PROD_REFRESH_URL.to_string())
}

/// reqwest 响应 → 原始透传(§18.6:上游 status/headers/body 原样)。
/// M7：unary 响应体严格上限 1 MiB（content-length 预检 + 实际读取复核）。
pub(crate) async fn raw_from_reqwest_response(
    response: reqwest::Response,
    _operation: Operation,
) -> Result<RawUpstreamResponse, LocalError> {
    const MAX_AUTH_BODY: usize = 1024 * 1024;
    if let Some(len) = response.content_length()
        && len > MAX_AUTH_BODY as u64
    {
        return Err(LocalError::new(
            "auth_body_too_large",
            ErrorClass::UpstreamProtocol,
            ErrorPhase::StreamBody,
            false,
            format!("auth response content-length {len} exceeds {MAX_AUTH_BODY}"),
        ));
    }
    let status = response.status();
    let headers = crate::contract::filter_upstream_headers(response.headers());
    // R4-H4：流式累计——每个 chunk 到达时先检查上限；chunked/无
    // Content-Length 响应也无法在检查前缓存超预算 body。
    let mut bytes = bytes::BytesMut::with_capacity(MAX_AUTH_BODY.min(16 * 1024));
    let mut stream = response.bytes_stream();
    use futures::TryStreamExt as _;
    while let Some(chunk) = stream.try_next().await.map_err(|err| {
        LocalError::new(
            "auth_body_read_failed",
            ErrorClass::Transport,
            ErrorPhase::StreamBody,
            false,
            format!("auth response body read failed: {err}"),
        )
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_AUTH_BODY {
            return Err(LocalError::new(
                "auth_body_too_large",
                ErrorClass::UpstreamProtocol,
                ErrorPhase::StreamBody,
                false,
                "auth response body exceeds 1 MiB limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let frozen = bytes.freeze();
    Ok(RawUpstreamResponse {
        status,
        headers,
        body: Box::pin(futures::stream::once(async move {
            Ok::<_, std::io::Error>(frozen)
        })),
        attempt_count: 1,
    })
}

#[allow(dead_code)]
fn _status_probe() -> StatusCode {
    StatusCode::OK
}

#[allow(dead_code)]
fn _headers_probe() -> HeaderMap {
    HeaderMap::new()
}

#[allow(dead_code)]
fn _bytes_probe() -> Bytes {
    Bytes::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_refresh_url_is_compile_time_constant() {
        // 非 test-upstream 下 refresh_endpoint 必须等于官方常量
        // （test 构建不带 feature，天然验证生产路径）。
        assert_eq!(refresh_endpoint().expect("constant"), PROD_REFRESH_URL);
    }

    #[cfg(feature = "test-upstream")]
    #[test]
    fn test_refresh_url_requires_loopback() {
        // R4-M2：进程级 env 变异串行化 + RAII 恢复（并行测试安全）。
        let _guard = crate::transport::env_mutex::env_guard_for("CTD_TEST_AUTH_REFRESH_URL");
        // Rust 2024：env::set_var 需 unsafe（测试专用）。
        unsafe {
            std::env::set_var(
                "CTD_TEST_AUTH_REFRESH_URL",
                "http://evil.example/oauth/token",
            )
        };
        let err = refresh_endpoint().expect_err("non-loopback must fail");
        assert_eq!(err.code, "test_auth_refresh_url_not_loopback");
        unsafe {
            std::env::set_var(
                "CTD_TEST_AUTH_REFRESH_URL",
                "http://127.0.0.1:9911/oauth/token",
            )
        };
        let url = refresh_endpoint().expect("loopback ok");
        assert!(url.starts_with("http://127.0.0.1:9911"));
    }
}
