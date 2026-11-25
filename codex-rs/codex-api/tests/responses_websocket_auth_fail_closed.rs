//! Fail-closed contract for the sidecar-selected websocket routes.
//!
//! `codex_api::AuthProvider::add_auth_headers` became fallible in
//! rust-v0.169.0 and its contract states that callers must not send the
//! request when it errors. Both fork-only constructors
//! ([`ResponsesWebsocketClient::connect_with_route`] and
//! `connect_raw_with_route`) must therefore abort before any handshake.

use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::Provider;
use codex_api::ResponsesWebsocketClient;
use codex_api::RetryConfig;
use codex_http_client::OutboundProxyRoute;
use http::HeaderMap;
use std::sync::Arc;
use std::time::Duration;

const AUTH_FAILURE: &str = "auth fixture boom";

/// `127.0.0.1:1` refuses connections immediately, so an implementation that
/// attempted the handshake first would surface a connection error instead of
/// the auth build error (the assertion below would then fail).
const UNREACHABLE_BASE_URL: &str = "http://127.0.0.1:1";

struct FailingAuth;

impl AuthProvider for FailingAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) -> Result<(), codex_api::AuthError> {
        Err(codex_api::AuthError::Build(AUTH_FAILURE.to_string()))
    }
}

fn failing_client() -> ResponsesWebsocketClient {
    ResponsesWebsocketClient::new(
        Provider {
            name: "auth fail-closed fixture".into(),
            base_url: UNREACHABLE_BASE_URL.to_string(),
            query_params: None,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(/*millis*/ 1),
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(/*secs*/ 5),
        },
        Arc::new(FailingAuth),
    )
}

fn assert_auth_failure(error: ApiError) {
    assert!(
        matches!(error, ApiError::Transport(_)),
        "expected an auth transport error, got {error}"
    );
    assert!(
        error.to_string().contains(AUTH_FAILURE),
        "auth error was not propagated: {error}"
    );
}

#[tokio::test]
async fn connect_with_route_fails_closed_when_auth_headers_fail() {
    let result = failing_client()
        .connect_with_route(
            OutboundProxyRoute::Direct,
            HeaderMap::new(),
            HeaderMap::new(),
            None,
            None,
        )
        .await;
    match result {
        Ok(_) => panic!("auth failure must abort before the handshake"),
        Err(error) => assert_auth_failure(error),
    }
}

#[tokio::test]
async fn connect_raw_with_route_fails_closed_when_auth_headers_fail() {
    let result = failing_client()
        .connect_raw_with_route(
            OutboundProxyRoute::Direct,
            HeaderMap::new(),
            HeaderMap::new(),
        )
        .await;
    match result {
        Ok(_) => panic!("auth failure must abort before the handshake"),
        Err(error) => assert_auth_failure(error),
    }
}
