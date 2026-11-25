use super::*;
use crate::request::Request;
use bytes::Bytes;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use serde_json::json;
use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
#[tokio::test]
async fn enabled_request_logging_emits_transport_url_and_body() {
    let logs = capture_transport_logs(
        HttpClient::new(test_reqwest_client()),
        LoggedRequestShape::Execute,
    )
    .await;

    assert!(logs.contains("log capture sentinel"));
    assert!(logs.contains("url-secret"));
    assert!(logs.contains("body-secret"));
}

#[tokio::test]
async fn disabled_request_logging_suppresses_transport_url_and_body() {
    let logs = capture_transport_logs(
        HttpClient::new(test_reqwest_client()).without_request_logging(),
        LoggedRequestShape::Execute,
    )
    .await;

    assert!(logs.contains("log capture sentinel"));
    assert!(!logs.contains("url-secret"));
    assert!(!logs.contains("body-secret"));

    let controller = crate::NetworkPolicyController::default();
    let policy = controller.policy();
    controller.publish(policy.revision(), crate::DestinationPolicy::Unrestricted);
    let factory = crate::HttpClientFactory::new(crate::OutboundProxyPolicy::ReqwestDefault)
        .with_network_policy(policy);
    let client =
        crate::RouteAwareClientPool::new(factory, crate::ClientRouteClass::Api).into_client();
    let logs = capture_transport_logs(client.clone(), LoggedRequestShape::Execute).await;
    assert!(logs.contains("url-secret"));
    let logs = capture_transport_logs(
        client.clone().without_request_logging(),
        LoggedRequestShape::Execute,
    )
    .await;
    assert!(!logs.contains("url-secret"), "{logs}");
    assert!(!logs.contains("body-secret"));
    assert!(
        capture_transport_logs(client, LoggedRequestShape::Execute)
            .await
            .contains("url-secret")
    );
}

#[tokio::test]
async fn disabled_request_logging_suppresses_execute_preserving_status_url_and_body() {
    let logs = capture_transport_logs(
        HttpClient::new(test_reqwest_client()).without_request_logging(),
        LoggedRequestShape::ExecutePreservingStatus,
    )
    .await;

    assert!(logs.contains("log capture sentinel"));
    assert!(!logs.contains("url-secret"));
    assert!(!logs.contains("body-secret"));
}

#[tokio::test]
async fn disabled_request_logging_suppresses_stream_preserving_status_url_and_body() {
    let logs = capture_transport_logs(
        HttpClient::new(test_reqwest_client()).without_request_logging(),
        LoggedRequestShape::StreamPreservingStatus,
    )
    .await;

    assert!(logs.contains("log capture sentinel"));
    assert!(!logs.contains("url-secret"));
    assert!(!logs.contains("body-secret"));
}

#[tokio::test]
async fn connection_failures_are_classified_without_exposing_request_urls() {
    let unavailable_server =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("server port should bind");
    let server_addr = unavailable_server
        .local_addr()
        .expect("server listener should have an address");
    drop(unavailable_server);
    let transport = ReqwestTransport::from_http_client(HttpClient::new(test_reqwest_client()));
    let request = Request::new(
        Method::POST,
        format!("http://{server_addr}/responses?token=url-secret"),
    );

    let error = match transport.stream(request).await {
        Err(TransportError::Connection(error)) => error,
        Err(error) => panic!("expected a connection failure, got {error}"),
        Ok(_) => panic!("an unavailable server should not return a response"),
    };
    assert!(!error.to_string().contains("url-secret"));
}

fn test_reqwest_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client should build")
}

#[derive(Clone, Copy)]
enum LoggedRequestShape {
    Execute,
    ExecutePreservingStatus,
    StreamPreservingStatus,
}

async fn capture_transport_logs(client: HttpClient, request_shape: LoggedRequestShape) -> String {
    let unavailable_server =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("server port should bind");
    let server_addr = unavailable_server
        .local_addr()
        .expect("server listener should have an address");
    drop(unavailable_server);
    let transport = ReqwestTransport::from_http_client(client);
    let log_buffer = Arc::new(Mutex::new(Vec::new()));
    let writer_buffer = Arc::clone(&log_buffer);
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || TestLogWriter(Arc::clone(&writer_buffer)))
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("codex_http_client::transport", tracing::Level::TRACE)
                    .with_target("codex_http_client::client", tracing::Level::DEBUG),
            ),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::trace!(target: "codex_http_client::transport", "log capture sentinel");
    let mut request = Request::new(
        Method::POST,
        format!("http://{server_addr}/request?token=url-secret"),
    )
    .with_json(&json!({"token": "body-secret"}));
    request.timeout = Some(Duration::from_secs(1));

    match request_shape {
        LoggedRequestShape::Execute => {
            let _ = transport.execute(request).await;
        }
        LoggedRequestShape::ExecutePreservingStatus => {
            let _ = transport.execute_preserving_status(request).await;
        }
        LoggedRequestShape::StreamPreservingStatus => {
            let _ = transport.stream_preserving_status(request).await;
        }
    }

    String::from_utf8(
        log_buffer
            .lock()
            .expect("log buffer should not be poisoned")
            .clone(),
    )
    .expect("captured logs should be UTF-8")
}

#[derive(Clone)]
struct TestLogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TestLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log buffer should not be poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// R-TRANSPORT-001（replay_0831 R6）raw 形态测试：
// `execute_preserving_status` / `stream_preserving_status` 对任何上游 status
// 返回 Ok，body 字节精确（含非 UTF-8 字节）；既有 `execute` / `stream` 的
// 非 2xx → `TransportError::Http` 映射保持不变。

/// 起一个一次性 mock 上游：每个连接回一个脚本化响应（connection: close）。
fn spawn_mock(
    script: Vec<(u16, Vec<(&'static str, &'static str)>, Vec<u8>)>,
) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let conns = Arc::clone(&connections);
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { break };
            let idx = conns.fetch_add(1, Ordering::SeqCst);
            let (status, headers, body) = script[idx.min(script.len() - 1)].clone();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut chunk).expect("read request head");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let reason = if status == 200 { "OK" } else { "Err" };
            let mut resp = format!("HTTP/1.1 {status} {reason}\r\n");
            for (name, value) in &headers {
                resp.push_str(&format!("{name}: {value}\r\n"));
            }
            resp.push_str(&format!("content-length: {}\r\n", body.len()));
            resp.push_str("connection: close\r\n\r\n");
            sock.write_all(resp.as_bytes()).expect("write head");
            sock.write_all(&body).expect("write body");
            let _ = sock.flush();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    });
    addr
}

fn test_transport() -> ReqwestTransport {
    ReqwestTransport::new(reqwest::Client::new())
}

fn get_request(url: String) -> Request {
    Request::new(Method::GET, url)
}

#[tokio::test]
async fn execute_preserving_status_keeps_403_status_headers_and_exact_bytes() {
    let addr = spawn_mock(vec![(
        403,
        vec![("x-upstream-trace", "t1")],
        "{\"error\":\"上游 403Σ\"}".as_bytes().to_vec(),
    )]);
    let transport = test_transport();
    let response = transport
        .execute_preserving_status(get_request(format!("http://{addr}/responses")))
        .await
        .expect("non-2xx must not become a transport error");
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.headers.get("x-upstream-trace").unwrap(), "t1");
    assert_eq!(
        response.body.as_ref(),
        "{\"error\":\"上游 403Σ\"}".as_bytes()
    );
}

#[tokio::test]
async fn execute_keeps_mapping_non2xx_to_http_error() {
    let addr = spawn_mock(vec![(403, vec![], b"denied".to_vec())]);
    let transport = test_transport();
    let err = transport
        .execute(get_request(format!("http://{addr}/responses")))
        .await
        .expect_err("existing execute keeps the error mapping");
    match err {
        crate::error::TransportError::Http { status, body, .. } => {
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body.as_deref(), Some("denied"));
        }
        other => panic!("expected TransportError::Http, got {other:?}"),
    }
}

#[tokio::test]
async fn stream_preserving_status_keeps_terminal_5xx_byte_true() {
    // 非 UTF-8 字节（0xFF/0x00）证明 body 不经文本往返。
    let body = vec![0xff, 0xfe, 0x00, b'b', 0x80];
    let addr = spawn_mock(vec![(500, vec![], body.clone())]);
    let transport = test_transport();
    let response = transport
        .stream_preserving_status(get_request(format!("http://{addr}/responses")))
        .await
        .expect("terminal 5xx must not become a transport error");
    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    let collected = drain(response.bytes).await;
    assert_eq!(collected, body);
}

#[tokio::test]
async fn stream_keeps_mapping_non2xx_to_http_error() {
    let addr = spawn_mock(vec![(500, vec![], b"boom".to_vec())]);
    let transport = test_transport();
    // StreamResponse 未实现 Debug：用 match 拆分，不施加 Debug 约束。
    let err = match transport
        .stream(get_request(format!("http://{addr}/responses")))
        .await
    {
        Ok(response) => panic!(
            "existing stream keeps the error mapping, got {}",
            response.status
        ),
        Err(err) => err,
    };
    match err {
        crate::error::TransportError::Http { status, .. } => {
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        }
        other => panic!("expected TransportError::Http, got {other:?}"),
    }
}

#[tokio::test]
async fn success_responses_flow_through_both_shapes_identically() {
    let addr = spawn_mock(vec![(200, vec![], b"ok".to_vec())]);
    let transport = test_transport();
    let response = transport
        .execute_preserving_status(get_request(format!("http://{addr}/responses")))
        .await
        .expect("2xx executes");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), b"ok");
}

async fn drain(stream: crate::transport::ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("stream chunk"));
    }
    out
}

/// HeaderMap/HeaderValue 导入使用防呆（该模块的 mock 响应头由字符串构造）。
#[test]
fn header_types_available() {
    let mut headers = HeaderMap::new();
    headers.insert("x-probe", HeaderValue::from_static("v"));
    assert_eq!(headers.get("x-probe").unwrap(), "v");
    let _bytes: Bytes = Bytes::from_static(b"x");
}
