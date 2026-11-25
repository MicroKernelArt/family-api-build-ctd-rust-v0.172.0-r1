//! Auxiliary operations（TLS-CTD-001 §12.4 / FR-015 / FR-026）。
//!
//! models/usage/quota/alpha 全部映射为封闭 operation，使用与核心
//! Responses 相同的官方 client factory、账号隔离和 proxy route；
//! alpha search 保持 session-less 官方形态，不注入 Responses metadata。

use bytes::Bytes;
use codex_client::HttpTransport;
use http::HeaderMap;
use http::Method;

use crate::contract::RawUpstreamResponse;
use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::transport::metadata;
use crate::transport::official_adapter;

/// Models client version 查询参数（官方 `client_version`）。
fn models_request_url(provider: &codex_api::Provider) -> String {
    codex_api::ModelsClient::<codex_client::ReqwestTransport>::request_url(
        provider,
        env!("CARGO_PKG_VERSION"),
    )
}

/// 执行 models 原始请求。
pub async fn execute_models_raw(
    pool: &crate::transport::client_pool::ClientPool,
    ctx: &RequestContext,
) -> Result<RawUpstreamResponse, LocalError> {
    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::Models,
    )
    .await?;
    let auth = official_adapter::official_auth(ctx);
    let provider = official_adapter::chatgpt_provider();
    let request_url = models_request_url(&provider);
    let api_client =
        codex_api::ModelsClient::new(official_adapter::transport_for(client), provider, auth);

    let mut headers = HeaderMap::new();
    if let Some(etag) = &ctx.if_none_match
        && let Ok(value) = http::HeaderValue::from_str(etag)
    {
        headers.insert(http::header::IF_NONE_MATCH, value);
    }

    let outcome = api_client
        .execute_raw(request_url, headers)
        .await
        .map_err(|err| crate::transport::responses::map_api_error(err, "models"))?;

    let mut raw = raw_from_response(outcome.value, Operation::Models)?;
    raw.attempt_count = u32::try_from(outcome.attempts).unwrap_or(u32::MAX);
    Ok(raw)
}

/// 执行 alpha search 原始请求（session-less）。
pub async fn execute_alpha_search_raw(
    pool: &crate::transport::client_pool::ClientPool,
    ctx: &RequestContext,
    body: Bytes,
) -> Result<RawUpstreamResponse, LocalError> {
    // F9（review）：alpha/search 与 responses/compact 一致地对重复 key
    // fail closed（round-3 H2 红线，serde_json 默认后者覆盖会静默改数据）。
    metadata::reject_duplicate_keys(std::str::from_utf8(&body).unwrap_or_default())?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        LocalError::new(
            "invalid_json",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "alpha/search body is not valid JSON",
        )
    })?;
    metadata::validate_body_object(&value)?;

    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::AlphaSearch,
    )
    .await?;
    let auth = official_adapter::official_auth(ctx);
    let provider = official_adapter::chatgpt_provider();
    let api_client =
        codex_api::SearchClient::new(official_adapter::transport_for(client), provider, auth);

    let outcome = api_client
        .execute_raw(value, HeaderMap::new())
        .await
        .map_err(|err| crate::transport::responses::map_api_error(err, "alpha_search"))?;

    let mut raw = raw_from_response(outcome.value, Operation::AlphaSearch)?;
    raw.attempt_count = u32::try_from(outcome.attempts).unwrap_or(u32::MAX);
    Ok(raw)
}

/// 执行 usage/quota 原始请求（WHAM 官方路径，无官方解析 client；
/// 使用官方 client factory + retry 语义）。
pub async fn execute_wham_raw(
    pool: &crate::transport::client_pool::ClientPool,
    ctx: &RequestContext,
    operation: Operation,
    body: Option<Bytes>,
) -> Result<RawUpstreamResponse, LocalError> {
    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        operation,
    )
    .await?;
    let auth = official_adapter::official_auth(ctx);
    let provider = official_adapter::wham_provider();

    let method = operation.upstream_method();
    let path = operation.upstream_path();
    let mut request = provider.build_request(method.clone(), path);
    if let Some(body) = body {
        // F9（review）：wham/usage 同样对重复 key fail closed（round-3 H2 红线）。
        metadata::reject_duplicate_keys(std::str::from_utf8(&body).unwrap_or_default())?;
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
            LocalError::new(
                "invalid_json",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "wham body is not valid JSON",
            )
        })?;
        request.body = Some(codex_client::RequestBody::Json(value));
    }

    let transport = official_adapter::transport_for(client);
    let retry_policy = provider.retry.to_policy();
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let attempts_handle = std::sync::Arc::clone(&attempts);
    let response = match codex_client::run_with_retry(
        retry_policy,
        || request.clone(),
        |req, attempt| {
            attempts_handle.store(attempt + 1, std::sync::atomic::Ordering::Relaxed);
            let auth = auth.clone();
            let transport = &transport;
            async move {
                let req = auth
                    .apply_auth(req)
                    .await
                    .map_err(codex_client::TransportError::from)?;
                transport.execute(req).await
            }
        },
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            // TransportError::Http 携带上游 status/headers/body：原样透传
            // （§18.6：403/404/429/5xx 状态/头/体原样），不是本地错误。
            match err {
                codex_client::TransportError::Http {
                    status,
                    headers,
                    body,
                    ..
                } => {
                    let headers = headers
                        .as_ref()
                        .map(crate::contract::filter_upstream_headers)
                        .unwrap_or_default();
                    // RF8（real-lane codexp）：按值匹配，body 直接 move 进 bytes，
                    // 不再先 clone 一份（超限响应在拒绝前不再重复占一份 body）。
                    let bytes = body.unwrap_or_default().into_bytes();
                    // RF5（real-lane codexp）：terminal 非 2xx 透传同样受 unary
                    // 响应上限约束，避免绕过 raw_from_response 的 post-buffer 上限。
                    if bytes.len() > MAX_UNARY_BODY {
                        return Err(LocalError::new(
                            "upstream_body_too_large",
                            ErrorClass::UpstreamProtocol,
                            ErrorPhase::StreamBody,
                            false,
                            format!(
                                "upstream non-2xx response body {} bytes exceeds unary limit {MAX_UNARY_BODY}",
                                bytes.len()
                            ),
                        ));
                    }
                    return Ok(RawUpstreamResponse {
                        status,
                        headers,
                        body: Box::pin(futures::stream::once(async move {
                            Ok::<_, std::io::Error>(bytes::Bytes::from(bytes))
                        })),
                        // R-OBS-001：terminal 非透传也携带真实 attempt 次数
                        //（retry 耗尽后的 429/5xx 不再硬编码 1）。
                        attempt_count: u32::try_from(
                            attempts.load(std::sync::atomic::Ordering::Relaxed),
                        )
                        .unwrap_or(u32::MAX),
                    });
                }
                other => {
                    return Err(crate::transport::responses::map_api_error(
                        codex_api::ApiError::Transport(other),
                        operation.metric_label(),
                    ));
                }
            }
        }
    };

    let mut raw = raw_from_response(response, operation)?;
    raw.attempt_count = attempts.load(std::sync::atomic::Ordering::Relaxed) as u32;
    Ok(raw)
}

/// unary 响应体上限（F10 review）：compact/wham/models/alpha 官方 client
/// 全量缓冲响应，需有界以防畸形/超大上游响应耗尽内存（auth 已有 1 MiB）。
const MAX_UNARY_BODY: usize = 128 * 1024 * 1024;

fn raw_from_response(
    response: codex_client::Response,
    _operation: Operation,
) -> Result<RawUpstreamResponse, LocalError> {
    let status = response.status;
    let headers = crate::contract::filter_upstream_headers(&response.headers);
    let bytes = response.body;
    if bytes.len() > MAX_UNARY_BODY {
        return Err(LocalError::new(
            "upstream_body_too_large",
            ErrorClass::UpstreamProtocol,
            ErrorPhase::StreamBody,
            false,
            format!(
                "upstream response body {} bytes exceeds unary limit {MAX_UNARY_BODY}",
                bytes.len()
            ),
        ));
    }
    Ok(RawUpstreamResponse {
        status,
        headers,
        body: Box::pin(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes)
        })),
        attempt_count: 1,
    })
}

/// 各 operation 的官方上游 method/path 快照（防逃生口回归）。
pub fn upstream_snapshot(operation: Operation) -> (Method, &'static str) {
    (operation.upstream_method(), operation.upstream_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_paths_are_wham_fixed() {
        assert_eq!(
            upstream_snapshot(Operation::Usage),
            (Method::GET, "wham/usage")
        );
        assert_eq!(
            upstream_snapshot(Operation::QuotaCredits),
            (Method::GET, "wham/rate-limit-reset-credits")
        );
        assert_eq!(
            upstream_snapshot(Operation::QuotaConsume),
            (Method::POST, "wham/rate-limit-reset-credits/consume")
        );
    }

    #[test]
    fn models_and_search_use_codex_base_paths() {
        assert_eq!(
            upstream_snapshot(Operation::Models),
            (Method::GET, "models")
        );
        assert_eq!(
            upstream_snapshot(Operation::AlphaSearch),
            (Method::POST, "alpha/search")
        );
    }
}
