//! UDS HTTP operation handlers（TLS-CTD-001 §11.3 / §11.6 / §16.2）。

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes as BytesType;

use crate::contract::operation::Operation;
use crate::limits;
use crate::server::ServerState;
use crate::server::execute_operation;
use crate::server::local_error_response;

/// 统一 HTTP operation 入口：admission → 校验 → 执行 → 原始流式回传。
pub(crate) async fn handle_http_operation(
    State(state): State<std::sync::Arc<ServerState>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let path = uri.path().to_string();
    // F-012：extractor rejection（body 超限/非法）统一映射为 typed 错误，
    // 保证所有本地拒绝响应带 canonical origin: local。
    let body = match body {
        Ok(body) => body,
        Err(_) => {
            return local_error_response(
                &crate::contract::error::codes::body_too_large(),
                "unknown",
            );
        }
    };
    let Some(operation) = Operation::from_internal_path(&method, &path) else {
        return local_error_response(
            &crate::contract::error::codes::unknown_operation(),
            "unknown",
        );
    };
    if operation == Operation::ResponsesWebSocketV2 {
        return local_error_response(
            &crate::contract::error::codes::unknown_operation(),
            "unknown",
        );
    }

    // 头部数量/字节上限（§16.2 顺序 3）。
    if headers.len() > limits::MAX_HEADER_COUNT || header_bytes(&headers) > limits::MAX_HEADER_BYTES
    {
        return local_error_response(
            &crate::contract::error::codes::missing_required_header("header limits exceeded")
                .code_override("header_limits_exceeded"),
            "unknown",
        );
    }
    if body.len() > limits::max_body_bytes() {
        return local_error_response(&crate::contract::error::codes::body_too_large(), "unknown");
    }

    let (request_id, _account_hash) = crate::server::extract_request_meta(&headers);
    let request_id = request_id.unwrap_or_else(|| "unknown".to_string());

    // 校验内部协议（版本/必需 header/session/代理），任何校验失败都在
    // DNS/proxy/upstream I/O 之前（§16.2 顺序 2–6）。
    let ctx = match crate::contract::headers::parse_request_context(&headers, operation) {
        Ok(ctx) => ctx,
        Err(err) => return local_error_response(&err, &request_id),
    };

    crate::observability::observe_request_start(
        &state.metrics,
        crate::observability::RequestObservation {
            operation,
            proxy_kind: ctx.proxy_kind,
            purpose: ctx.purpose,
            hookpoint: ctx.hookpoint,
        },
    );

    let observation = crate::observability::RequestObservation {
        operation,
        proxy_kind: ctx.proxy_kind,
        purpose: ctx.purpose,
        hookpoint: ctx.hookpoint,
    };
    let result = execute_operation(&state, &ctx, body).await;
    match result {
        Ok(response) => {
            // H4：Success/inflight 结算移入 GuardedBody（body EOF/error/drop
            // 时进行）——headers 后流式错误不再统计为成功。
            response
        }
        Err(err) => {
            crate::observability::observe_error(
                &state.metrics,
                observation,
                err.class.as_str(),
                err.phase.as_str(),
            );
            crate::observability::observe_request_end(
                &state.metrics,
                crate::observability::RequestObservation {
                    operation,
                    proxy_kind: ctx.proxy_kind,
                    purpose: ctx.purpose,
                    hookpoint: ctx.hookpoint,
                },
                crate::observability::Outcome::LocalError,
            );
            local_error_response(&err, &request_id)
        }
    }
}

fn header_bytes(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
        .sum()
}

/// 把原始上游响应转成 internal 响应：status/headers 原样，body 逐块流式；
/// `X-CTD-Origin: upstream` 只供 Go 内部消费。
pub(crate) fn upstream_response(
    raw: crate::contract::RawUpstreamResponse,
    ctx: &crate::contract::RequestContext,
) -> Response {
    let _ = ctx;
    let mut builder = Response::builder().status(raw.status);
    for (name, value) in &raw.headers {
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    builder = builder.header("x-ctd-origin", "upstream");
    builder
        .body(Body::from_stream(raw.body))
        .expect("valid upstream response")
}

pub(crate) async fn health_live() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("ok"))
        .expect("valid response")
}

pub(crate) async fn health_ready(State(state): State<std::sync::Arc<ServerState>>) -> Response {
    // installation 已解析；socket/limits 正常即 ready。
    if state.installation_id.is_empty() {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("installation not resolved"))
            .expect("valid response");
    }
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("ok"))
        .expect("valid response")
}

pub(crate) async fn info(State(state): State<std::sync::Arc<ServerState>>) -> Response {
    let json = crate::server::info_json(&state);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json).unwrap_or_default()))
        .expect("valid response")
}

/// capture 运行状态（capture-r1；新增附加路由，/info 契约字节形状不变）。
/// 仅暴露计数器与使能位：无任何用户/会话/请求标识、无捕获内容。
pub(crate) async fn capture_status(State(state): State<std::sync::Arc<ServerState>>) -> Response {
    let capture = crate::capture::global();
    let json = serde_json::json!({
        "enabled": capture.is_some(),
        "dropped_total": state.metrics.capture_dropped_total.load(std::sync::atomic::Ordering::Relaxed),
        "truncated_total": state
            .metrics
            .capture_truncated_total
            .load(std::sync::atomic::Ordering::Relaxed),
        "write_failures_total": state
            .metrics
            .capture_write_failures_total
            .load(std::sync::atomic::Ordering::Relaxed),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json).unwrap_or_default()))
        .expect("valid response")
}

/// 工具 trait：LocalError 覆盖 code 字段（限制类错误保留类/相位）。
trait CodeOverride {
    fn code_override(self, code: &'static str) -> crate::contract::error::LocalError;
}

impl CodeOverride for crate::contract::error::LocalError {
    fn code_override(mut self, code: &'static str) -> crate::contract::error::LocalError {
        self.code = code;
        self
    }
}

#[allow(dead_code)]
fn _bytes_probe() -> BytesType {
    BytesType::new()
}
