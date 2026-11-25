//! R-TRANSPORT-001 / R-OBS-001（replay_0831 R6）：raw 重试循环语义。
//!
//! official retry 语义保持：429/5xx 按 policy 重试；403 等终态立即返回；
//! 预算耗尽返回最后一个响应并携带真实 attempt 次数。

use std::sync::Arc;
use std::time::Duration;

use codex_client::RawRetryOutcome;
use codex_client::Request;
use codex_client::RetryOn;
use codex_client::RetryPolicy;
use codex_client::run_with_retry_preserving_status;
use http::Method;
use http::StatusCode;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeResponse {
    status: u16,
}

/// 脚本项：`TransportError` 非 Clone，用可构造的枚举描述脚本。
enum StepScript {
    Resp(u16),
    Timeout,
}

/// 脚本化 op：每次调用按序消耗一项，耗尽后停在最后一项。
fn scripted(steps: Vec<StepScript>) -> impl FnMut(Request, u64) -> std::future::Ready<StepOut> {
    let script = Arc::new(steps);
    let mut next = 0usize;
    move |_req, _attempt| {
        let index = next.min(script.len() - 1);
        next += 1;
        std::future::ready(match &script[index] {
            StepScript::Resp(status) => Ok(FakeResponse { status: *status }),
            StepScript::Timeout => Err(codex_client::TransportError::Timeout),
        })
    }
}

type StepOut = Result<FakeResponse, codex_client::TransportError>;

fn policy(max_attempts: u64) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(1),
        retry_on: RetryOn {
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        },
    }
}

fn request() -> Request {
    Request::new(Method::POST, "http://127.0.0.1:9/unused".to_string())
}

fn classify(response: &FakeResponse) -> Option<StatusCode> {
    StatusCode::from_u16(response.status).ok()
}

#[tokio::test]
async fn non_retryable_status_returns_immediately_with_exact_attempts() {
    let outcome = run_with_retry_preserving_status(
        policy(4),
        request,
        scripted(vec![StepScript::Resp(403)]),
        classify,
    )
    .await
    .expect("terminal 403 must be returned, not mapped to an error");
    assert_eq!(
        outcome,
        RawRetryOutcome {
            value: FakeResponse { status: 403 },
            attempts: 1,
        }
    );
}

#[tokio::test]
async fn retryable_then_success_reports_exact_attempts() {
    let outcome = run_with_retry_preserving_status(
        policy(4),
        request,
        scripted(vec![StepScript::Resp(500), StepScript::Resp(200)]),
        classify,
    )
    .await
    .expect("success after retry");
    assert_eq!(
        outcome,
        RawRetryOutcome {
            value: FakeResponse { status: 200 },
            attempts: 2,
        }
    );
}

#[tokio::test]
async fn exhausted_retry_budget_returns_last_response_with_exact_attempts() {
    // 官方循环语义：attempt ∈ 0..=max_attempts → 总尝试 = max_attempts + 1
    // （max_attempts=4 → 5 次尝试 / 4 次 retry），与 transportd wham 的
    // attempts 原子计数一致。
    let outcome = run_with_retry_preserving_status(
        policy(4),
        request,
        scripted(vec![
            StepScript::Resp(500),
            StepScript::Resp(500),
            StepScript::Resp(500),
            StepScript::Resp(500),
        ]),
        classify,
    )
    .await
    .expect("terminal 500 must survive retry exhaustion");
    assert_eq!(
        outcome,
        RawRetryOutcome {
            value: FakeResponse { status: 500 },
            attempts: 5,
        }
    );
}

#[tokio::test]
async fn transport_errors_follow_official_retry_semantics() {
    // retry_transport = true：Timeout 重试直到预算耗尽 → 最后一个错误返回
    // （max_attempts=2 → 共 3 次 attempt）。
    let outcome = run_with_retry_preserving_status(
        policy(2),
        request,
        scripted(vec![StepScript::Timeout]),
        classify,
    )
    .await;
    assert!(
        matches!(outcome, Err(codex_client::TransportError::Timeout)),
        "timeout must surface as-is after budget exhaustion"
    );
}

#[tokio::test]
async fn no_retry_status_classifier_skips_response_retries() {
    // 对照：classifier 返回 None 时绝不因响应重试。
    let outcome = run_with_retry_preserving_status(
        policy(4),
        request,
        |_req, _attempt| {
            std::future::ready(Ok::<FakeResponse, codex_client::TransportError>(
                FakeResponse { status: 500 },
            ))
        },
        |_response| None,
    )
    .await
    .expect("classifier None → immediate return");
    assert_eq!(outcome.attempts, 1);
}

/// `RetryOn::should_retry_status` 与既有 `should_retry` 的错误路径判定
/// 保持一致（同一 policy 对同一状态只翻转载体形态）。
#[test]
fn should_retry_status_matches_error_path_semantics() {
    let retry_on = RetryOn {
        retry_429: false,
        retry_5xx: true,
        retry_transport: true,
    };
    assert!(retry_on.should_retry_status(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(!retry_on.should_retry_status(StatusCode::TOO_MANY_REQUESTS));
    assert!(!retry_on.should_retry_status(StatusCode::FORBIDDEN));
    let err = codex_client::TransportError::Http {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        url: None,
        headers: None,
        body: None,
        // 0.159.0 新增字段：错误路径重试判定不看 retry advice。
        retry_after: None,
    };
    assert!(retry_on.should_retry(&err, 0, 4));
}
