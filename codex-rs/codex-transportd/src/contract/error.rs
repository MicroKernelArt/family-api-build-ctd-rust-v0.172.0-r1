//! 内部协议错误 schema（TLS-CTD-001 §11.6）。
//!
//! sidecar 本地错误统一为 `application/problem+json`：
//! `code/class/phase/retryable/response_started/request_id/message`。
//! 错误正文永不包含 token、proxy credential、body、session 值或 debug chain。

use http::StatusCode;
use serde::Serialize;

/// 允许的错误 class（§11.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Contract,
    Config,
    Capacity,
    Transport,
    Timeout,
    UpstreamProtocol,
    Internal,
}

impl ErrorClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Contract => "contract",
            Self::Config => "config",
            Self::Capacity => "capacity",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::UpstreamProtocol => "upstream_protocol",
            Self::Internal => "internal",
        }
    }
}

/// 允许的错误 phase（§11.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPhase {
    Admission,
    Validate,
    ResolveProxy,
    Connect,
    Tls,
    WriteRequest,
    WaitHeaders,
    StreamBody,
    WsHandshake,
    WsRelay,
}

impl ErrorPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Validate => "validate",
            Self::ResolveProxy => "resolve_proxy",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::WriteRequest => "write_request",
            Self::WaitHeaders => "wait_headers",
            Self::StreamBody => "stream_body",
            Self::WsHandshake => "ws_handshake",
            Self::WsRelay => "ws_relay",
        }
    }
}

/// 带错误上下文的本地错误。
#[derive(Debug, Clone)]
pub struct LocalError {
    pub code: &'static str,
    pub class: ErrorClass,
    pub phase: ErrorPhase,
    pub retryable: bool,
    pub response_started: bool,
    pub message: String,
}

impl LocalError {
    pub fn new(
        code: &'static str,
        class: ErrorClass,
        phase: ErrorPhase,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            class,
            phase,
            retryable,
            response_started: false,
            message: message.into(),
        }
    }

    pub fn with_response_started(mut self) -> Self {
        self.response_started = true;
        self
    }

    /// 问题响应对应的 HTTP status。
    pub fn status(&self) -> StatusCode {
        match self.class {
            ErrorClass::Contract | ErrorClass::Config => StatusCode::BAD_REQUEST,
            ErrorClass::Capacity => {
                if self.retryable {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                }
            }
            ErrorClass::Timeout => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

/// problem+json 响应体（序列化后不包含任何 secret）。
#[derive(Debug, Serialize)]
pub struct ProblemBody<'a> {
    pub code: &'a str,
    pub class: &'a str,
    pub phase: &'a str,
    pub retryable: bool,
    pub response_started: bool,
    pub request_id: &'a str,
    pub message: &'a str,
}

impl LocalError {
    pub fn problem_body<'a>(&'a self, request_id: &'a str) -> ProblemBody<'a> {
        ProblemBody {
            code: self.code,
            class: self.class.as_str(),
            phase: self.phase.as_str(),
            retryable: self.retryable,
            response_started: self.response_started,
            request_id,
            message: &self.message,
        }
    }
}

/// 便捷构造：常见本地错误。
pub mod codes {
    use super::*;

    pub fn invalid_protocol_version() -> LocalError {
        LocalError::new(
            "invalid_protocol_version",
            ErrorClass::Contract,
            ErrorPhase::Admission,
            false,
            "unsupported X-CTD-Protocol-Version",
        )
    }

    pub fn unknown_operation() -> LocalError {
        LocalError::new(
            "unknown_operation",
            ErrorClass::Contract,
            ErrorPhase::Admission,
            false,
            "unknown operation or method/path combination",
        )
    }

    pub fn missing_required_header(name: &str) -> LocalError {
        LocalError::new(
            "missing_required_header",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            format!("missing required header {name}"),
        )
    }

    pub fn invalid_session() -> LocalError {
        LocalError::new(
            "invalid_session",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "session snapshot failed validation",
        )
    }

    pub fn reserved_metadata_conflict() -> LocalError {
        LocalError::new(
            "reserved_metadata_conflict",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "request carries reserved metadata keys owned by the sidecar",
        )
    }

    pub fn invalid_json() -> LocalError {
        LocalError::new(
            "invalid_json",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "request body is not a valid JSON object",
        )
    }

    pub fn body_too_large() -> LocalError {
        LocalError::new(
            "body_too_large",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "request body exceeds the configured limit",
        )
    }

    pub fn unsupported_proxy_kind() -> LocalError {
        LocalError::new(
            "unsupported_proxy_kind",
            ErrorClass::Config,
            ErrorPhase::ResolveProxy,
            false,
            "proxy kind is not supported; no fallback to direct",
        )
    }

    pub fn invalid_proxy_url() -> LocalError {
        LocalError::new(
            "invalid_proxy_url",
            ErrorClass::Config,
            ErrorPhase::ResolveProxy,
            false,
            "proxy URL failed validation",
        )
    }

    pub fn admission_rejected() -> LocalError {
        LocalError::new(
            "admission_rejected",
            ErrorClass::Capacity,
            ErrorPhase::Admission,
            true,
            "sidecar admission queue is full",
        )
    }

    pub fn upstream_connect_failed(message: impl Into<String>) -> LocalError {
        LocalError::new(
            "upstream_connect_failed",
            ErrorClass::Transport,
            ErrorPhase::Connect,
            true,
            message,
        )
    }

    pub fn proxy_connect_failed(message: impl Into<String>) -> LocalError {
        LocalError::new(
            "proxy_connect_failed",
            ErrorClass::Transport,
            ErrorPhase::ResolveProxy,
            true,
            message,
        )
    }

    pub fn upstream_error(message: impl Into<String>) -> LocalError {
        LocalError::new(
            "upstream_error",
            ErrorClass::Transport,
            ErrorPhase::WaitHeaders,
            false,
            message,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_body_contains_no_secrets() {
        let err = codes::invalid_session();
        let body = err.problem_body("req_123");
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(json.contains("req_123"));
        assert!(json.contains("invalid_session"));
        assert!(!json.to_lowercase().contains("token"));
    }

    #[test]
    fn capacity_maps_to_429_or_503() {
        assert_eq!(
            codes::admission_rejected().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let non_retryable = LocalError::new(
            "queue_full",
            ErrorClass::Capacity,
            ErrorPhase::Admission,
            false,
            "x",
        );
        assert_eq!(non_retryable.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn response_started_flag_roundtrips() {
        let err = codes::upstream_connect_failed("x").with_response_started();
        assert!(err.response_started);
        let body = err.problem_body("req");
        assert!(body.response_started);
    }
}
