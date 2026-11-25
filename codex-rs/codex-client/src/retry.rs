use codex_http_client::Request;
use codex_http_client::RetryAfter;
use codex_http_client::TransportError;
use http::StatusCode;
use rand::Rng;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u64,
    pub base_delay: Duration,
    pub retry_on: RetryOn,
}

#[derive(Debug, Clone)]
pub struct RetryOn {
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryOn {
    pub fn should_retry(&self, err: &TransportError, attempt: u64, max_attempts: u64) -> bool {
        if attempt >= max_attempts {
            return false;
        }
        match err {
            TransportError::Http { status, .. } => self.should_retry_status(*status),
            TransportError::Timeout
            | TransportError::Connection(_)
            | TransportError::Network(_) => self.retry_transport,
            TransportError::Build(_)
            | TransportError::RetryLimit
            | TransportError::Policy(_)
            | TransportError::ResponseTooLarge { .. } => false,
        }
    }

    /// Raw 路径重试判定：非 2xx 以 Ok(response) 形态返回时按状态判定。
    /// attempt 边界由调用方（重试循环）负责。
    pub fn should_retry_status(&self, status: StatusCode) -> bool {
        (self.retry_429 && status.as_u16() == 429) || (self.retry_5xx && status.is_server_error())
    }
}

pub fn backoff(base: Duration, attempt: u64) -> Duration {
    if attempt == 0 {
        return base;
    }
    let exp = 2u64.saturating_pow(attempt as u32 - 1);
    let millis = base.as_millis() as u64;
    let raw = millis.saturating_mul(exp);
    let jitter: f64 = rand::rng().random_range(0.9..1.1);
    Duration::from_millis((raw as f64 * jitter) as u64)
}

/// Identifies a retry path and its associated trace-event layer.
#[derive(Debug, Clone, Copy)]
pub enum RetryOperation {
    HttpRequest,
    Sampling,
    RemoteCompactionV2,
}

/// Emits retry telemetry at the caller's source location without adding it to normal OTEL logs.
#[macro_export]
macro_rules! record_retry {
    ($attempt:expr, $delay:expr, $operation:expr $(,)?) => {{
        let (layer, operation) = match $operation {
            $crate::RetryOperation::HttpRequest => ("http", "request"),
            $crate::RetryOperation::Sampling => ("stream", "sampling"),
            $crate::RetryOperation::RemoteCompactionV2 => ("stream", "remote_compaction_v2"),
        };

        ::tracing::event!(
            target: "codex_otel.trace_safe",
            ::tracing::Level::TRACE,
            event.name = "codex.retry",
            retry.attempt = $attempt,
            retry.delay_ms = ($delay).as_millis() as u64,
            retry.layer = layer,
            retry.operation = operation,
        );
    }};
}

pub async fn run_with_retry<T, F, Fut>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    op: F,
) -> Result<T, TransportError>
where
    F: Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    for attempt in 0..=policy.max_attempts {
        let req = make_req();
        match op(req, attempt).await {
            Ok(resp) => return Ok(resp),
            Err(err)
                if policy
                    .retry_on
                    .should_retry(&err, attempt, policy.max_attempts) =>
            {
                let retry_attempt = attempt + 1;
                let retry_after = err.retry_after();
                let delay = retry_after
                    .map(RetryAfter::remaining_delay)
                    .unwrap_or_else(|| backoff(policy.base_delay, retry_attempt));
                crate::record_retry!(retry_attempt, delay, RetryOperation::HttpRequest);
                if let Some(retry_after) = retry_after {
                    tokio::time::sleep_until(retry_after.deadline()).await;
                } else {
                    tokio::time::sleep(delay).await;
                }
            }
            Err(err) => return Err(err),
        }
    }
    Err(TransportError::RetryLimit)
}

/// raw 重试循环的终态：最后一次响应原样返回，并携带真实 attempt 总数
/// （>= 1）。transportd 用它回填 `RawUpstreamResponse.attempt_count` 与
/// bounded-cardinality retry 指标，不再硬编码 1。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRetryOutcome<T> {
    pub value: T,
    pub attempts: u64,
}

/// raw 重试循环（transportd 专用，TLS-CTD-001 R6）：
/// - `response_status(&T) -> Option<StatusCode>` 返回 `Some` 时按官方
///   `RetryOn::should_retry_status` 判定是否重试（429/5xx）；
/// - retry 预算耗尽或非 retryable 终态：最后一个响应**原样返回**
///   （status/headers/body 不丢失），`attempts` 为真实尝试次数；
/// - 传输层错误（Timeout/Network）语义与 `run_with_retry` 完全一致。
///
/// 只在明确 retryable 的状态上 replay；403 等终态绝不重发。
pub async fn run_with_retry_preserving_status<T, F, Fut, C>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    mut op: F,
    response_status: C,
) -> Result<RawRetryOutcome<T>, TransportError>
where
    F: FnMut(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
    C: Fn(&T) -> Option<StatusCode>,
{
    for attempt in 0..=policy.max_attempts {
        let req = make_req();
        match op(req, attempt).await {
            Ok(value) => {
                let retry = response_status(&value).is_some_and(|status| {
                    attempt < policy.max_attempts && policy.retry_on.should_retry_status(status)
                });
                if retry {
                    tokio::time::sleep(backoff(policy.base_delay, attempt + 1)).await;
                    continue;
                }
                return Ok(RawRetryOutcome {
                    value,
                    attempts: attempt + 1,
                });
            }
            Err(err)
                if policy
                    .retry_on
                    .should_retry(&err, attempt, policy.max_attempts) =>
            {
                tokio::time::sleep(backoff(policy.base_delay, attempt + 1)).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(TransportError::RetryLimit)
}
