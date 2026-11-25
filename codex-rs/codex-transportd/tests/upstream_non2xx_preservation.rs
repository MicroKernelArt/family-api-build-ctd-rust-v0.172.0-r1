//! R-TRANSPORT-001（replay_0831 R6）：terminal upstream non-2xx 原样保留。
//!
//! 403/429/终态 5xx 必须把上游 status/headers/body 原样带回 UDS 调用方，
//! 并带 `x-ctd-origin: upstream`；官方 retry 语义保持不变（5xx 重试，
//! 403/429 终态），replay 只发生在明确 retryable 状态上。
//!
//! 隔离：沿用 capture_upstream_contract 的 harness——`CTD_TEST_UPSTREAM_BASE_URL`
//! 是进程级 override（仅 test-upstream feature 生效），真实请求在自孵化
//! 子进程中执行：父进程持有 mock 上游并统计连接数，子进程只拿 env。
//! 父进程零 env 变更。

#![cfg(feature = "test-upstream")]

use std::io::Read;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// worker 测试名（子进程 --exact 过滤）。
const WORKER_TEST: &str = "non2xx_preservation_worker";
/// 场景 env：`<uds|direct>|<operation>|<script>|<expected_attempts>`。
const SCENARIO_ENV: &str = "CTD_NON2XX_SCENARIO";
const CHILD_ENV: &str = "CTD_NON2XX_CHILD";

// ---------------------------------------------------------------------------
// 场景表
// ---------------------------------------------------------------------------

// 403 body 携带一个非 ASCII UTF-8 序列（Σ = \xCE\xA3），证明字节精确透传
// （不得经过 String 往返）。
const BODY_403: &[u8] =
    b"{\"error\":{\"code\":\"forbidden\",\"message\":\"upstream 403 \xCE\xA3\"}}";
const BODY_429: &[u8] = b"{\"error\":{\"code\":\"rate_limited\"}}";
const BODY_500: &[u8] = b"{\"error\":{\"code\":\"boom\"}}";
const BODY_SSE_OK: &[u8] = b"retry: 0\n\n";
const BODY_304: &[u8] = b"";

/// `(status, body)` 序列：按连接次数消费，超出后停留在最后一项。
fn script_for(name: &str) -> Vec<(u16, &'static [u8])> {
    match name {
        "403" => vec![(403, BODY_403)],
        "304" => vec![(304, BODY_304)],
        "429" => vec![(429, BODY_429)],
        "500x4" => vec![
            (500, BODY_500),
            (500, BODY_500),
            (500, BODY_500),
            (500, BODY_500),
        ],
        "500_200" => vec![(500, BODY_500), (200, BODY_SSE_OK)],
        other => panic!("unknown script {other}"),
    }
}

fn expected_status(script: &str) -> u16 {
    match script {
        "403" => 403,
        "304" => 304,
        "429" => 429,
        // 5xx 在官方 retry（max_attempts=4, retry_5xx）耗尽后原样返回。
        "500x4" => 500,
        "500_200" => 200,
        other => panic!("unknown script {other}"),
    }
}

fn expected_body(script: &str) -> &'static [u8] {
    match script {
        "403" => BODY_403,
        "304" => BODY_304,
        "429" => BODY_429,
        "500x4" => BODY_500,
        "500_200" => BODY_SSE_OK,
        other => panic!("unknown script {other}"),
    }
}

// ---------------------------------------------------------------------------
// mock 上游（父进程持有；按连接计数消费脚本）
// ---------------------------------------------------------------------------

struct MockUpstream {
    addr: std::net::SocketAddr,
    connections: Arc<AtomicUsize>,
}

fn spawn_mock(script: Vec<(u16, &'static [u8])>) -> MockUpstream {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let conns = Arc::clone(&connections);
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { break };
            let idx = conns.fetch_add(1, Ordering::SeqCst);
            let (status, body) = script[idx.min(script.len() - 1)];
            let head = read_request_head_and_body(&mut sock);
            assert!(!head.is_empty(), "mock upstream got an empty request");
            if status == 304 {
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                assert!(
                    head.contains("if-none-match: \"models-etag\""),
                    "304 models request must forward If-None-Match: {head}"
                );
            }
            let reason = if status == 200 { "OK" } else { "Error" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes())
                .expect("write response head");
            sock.write_all(body).expect("write response body");
            let _ = sock.flush();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    });
    MockUpstream { addr, connections }
}

/// 读一个完整请求（head + content-length body），内容只用于吞吐，不校验。
fn read_request_head_and_body(sock: &mut std::net::TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).expect("read request");
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        assert!(buf.len() < 1024 * 1024, "request exceeds bound");
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let content_length = head
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + content_length {
        let n = sock.read(&mut chunk).expect("read request body");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    buf
}

// ---------------------------------------------------------------------------
// 父进程：逐场景孵化子进程
// ---------------------------------------------------------------------------

fn run_child(scenario: &str, mock: &MockUpstream) {
    let exe = std::env::current_exe().expect("current test binary");
    let out = Command::new(exe)
        .args([WORKER_TEST, "--exact", "--test-threads=1"])
        .env(
            "CTD_TEST_UPSTREAM_BASE_URL",
            format!("http://{}", mock.addr),
        )
        .env(SCENARIO_ENV, scenario)
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn worker child");
    assert!(
        out.status.success(),
        "worker failed for scenario {scenario}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// UDS 端到端（TLS-CTD-001 §11.1）：403/429/终态 5xx 通过 serve() 全链路
/// 原样回传给 UDS 调用方，带 x-ctd-origin: upstream。
#[test]
fn uds_preserves_terminal_403_429_and_exhausted_5xx() {
    for scenario in [
        "uds|responses|403|1",
        "uds|responses|429|1",
        "uds|responses|500x4|5",
        "uds|compact|403|1",
        "uds|models|403|1",
        "uds|models|304|1",
    ] {
        let script = script_for(scenario.split('|').nth(2).expect("script field"));
        let mock = spawn_mock(script);
        run_child(scenario, &mock);
        let attempts = mock.connections.load(Ordering::SeqCst);
        let expected_attempts: usize = scenario
            .split('|')
            .nth(3)
            .expect("attempts")
            .parse()
            .expect("attempts num");
        assert_eq!(
            attempts, expected_attempts,
            "scenario {scenario}: mock upstream connection count must equal attempts"
        );
    }
}

/// 直连适配层：真实 retry 语义 + 终态保留 + 真实 attempt 计数
/// （responses SSE 流与 compact 缓冲两种形态）。
#[test]
fn direct_preserves_retry_then_success_and_exhaustion() {
    for scenario in [
        "direct|responses|500_200|2",
        "direct|responses|500x4|5",
        "direct|responses|403|1",
        "direct|compact|500_200|2",
        "direct|compact|500x4|5",
        "direct|models|403|1",
        "direct|search|403|1",
    ] {
        let script = script_for(scenario.split('|').nth(2).expect("script field"));
        let mock = spawn_mock(script);
        run_child(scenario, &mock);
        let attempts = mock.connections.load(Ordering::SeqCst);
        let expected_attempts: usize = scenario
            .split('|')
            .nth(3)
            .expect("attempts")
            .parse()
            .expect("attempts num");
        assert_eq!(
            attempts, expected_attempts,
            "scenario {scenario}: mock upstream connection count must equal attempts"
        );
    }
}

// ---------------------------------------------------------------------------
// 子进程 worker
// ---------------------------------------------------------------------------

fn worker_scenario() -> (String, String, String) {
    let raw = std::env::var(SCENARIO_ENV).unwrap_or_default();
    let mut parts = raw.split('|');
    let mode = parts.next().unwrap_or_default().to_string();
    let op = parts.next().unwrap_or_default().to_string();
    let script = parts.next().unwrap_or_default().to_string();
    assert_eq!(
        std::env::var(CHILD_ENV).as_deref(),
        Ok("1"),
        "worker must run as the spawned child, never in-process"
    );
    // 双保险：override 只允许指向 loopback，绝无打到官方端点的可能。
    let base = std::env::var("CTD_TEST_UPSTREAM_BASE_URL").unwrap_or_default();
    let parsed = url::Url::parse(&base).expect("base url parses");
    assert_eq!(parsed.scheme(), "http", "mock is plain loopback http");
    assert!(
        matches!(
            parsed.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
        ),
        "mock base must be loopback, got {base}"
    );
    (mode, op, script)
}

fn worker_ctx(
    operation: codex_transportd::contract::operation::Operation,
) -> codex_transportd::contract::RequestContext {
    codex_transportd::contract::RequestContext {
        request_id: "req-non2xx".to_string(),
        account_key: "acct".to_string(),
        credential_revision: "rev-1".to_string(),
        access_token: codex_transportd::redaction::SecretString::new("test-token"),
        chatgpt_account_id: None,
        operation,
        proxy_kind: codex_transportd::contract::ProxyKind::Direct,
        proxy_url: None,
        session: Some(codex_transportd::contract::headers::SessionSnapshot {
            session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
            thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
            window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        }),
        purpose: codex_transportd::contract::RequestPurpose::Request,
        ..Default::default()
    }
}

fn worker_body() -> bytes::Bytes {
    serde_json::json!({
        "model": "gpt-5.2-non2xx",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "non2xx-marker"}],
        }],
    })
    .to_string()
    .into_bytes()
    .into()
}

async fn drain_body(body: &mut codex_transportd::contract::BodyStream) -> Vec<u8> {
    use futures::StreamExt;
    let mut received = Vec::new();
    while let Some(chunk) = body.next().await {
        received.extend_from_slice(&chunk.expect("body chunk"));
    }
    received
}

#[tokio::test(flavor = "multi_thread")]
async fn non2xx_preservation_worker() {
    if std::env::var("CTD_NON2XX_CHILD").is_err() {
        // 主套件直跑占位：真实逻辑仅由父测试 spawn 的 --exact 子进程执行。
        return;
    }
    // serve() 的启动失败经 tracing 记录；装上 subscriber 让子进程 stderr
    // 可诊断（默认测试进程无 subscriber，日志全丢）。
    let _ = tracing_subscriber::fmt::try_init();
    let (mode, op, script) = worker_scenario();
    let pool = codex_transportd::transport::client_pool::ClientPool::new(Arc::new(
        codex_transportd::observability::Metrics::default(),
    ));

    let expected_status = expected_status(&script);
    let expected_body = expected_body(&script);
    match mode.as_str() {
        "direct" => {
            let operation = match op.as_str() {
                "responses" => codex_transportd::contract::operation::Operation::Responses,
                "compact" => codex_transportd::contract::operation::Operation::ResponsesCompact,
                "models" => codex_transportd::contract::operation::Operation::Models,
                "search" => codex_transportd::contract::operation::Operation::AlphaSearch,
                other => panic!("unknown direct operation {other}"),
            };
            let mut ctx = worker_ctx(operation);
            if script == "304" {
                ctx.if_none_match = Some("\"models-etag\"".to_string());
            }
            let body = worker_body();
            let result = match op.as_str() {
                "responses" => {
                    codex_transportd::transport::responses::execute_responses_raw(
                        &pool,
                        "installation-1",
                        &ctx,
                        body,
                    )
                    .await
                }
                "compact" => {
                    codex_transportd::transport::compact::execute_compact_raw(
                        &pool,
                        "installation-1",
                        &ctx,
                        body,
                    )
                    .await
                }
                "models" => {
                    codex_transportd::transport::auxiliary::execute_models_raw(&pool, &ctx).await
                }
                "search" => {
                    codex_transportd::transport::auxiliary::execute_alpha_search_raw(
                        &pool, &ctx, body,
                    )
                    .await
                }
                other => panic!("unknown direct operation {other}"),
            };
            let mut raw = result.unwrap_or_else(|err| {
                panic!("terminal upstream response must be preserved: {err:?}")
            });
            assert_eq!(
                raw.status.as_u16(),
                expected_status,
                "op {op} script {script}"
            );
            let body = drain_body(&mut raw.body).await;
            assert_eq!(
                body, expected_body,
                "op {op} script {script}: body must be byte-true"
            );
            // raw 层 header 只来自上游投影；x-ctd-* 命名空间必须剥离。
            assert!(raw.headers.get("x-ctd-origin").is_none());
        }
        "uds" => {
            let (status, headers, body) = uds_round_trip(&op, &script).await;
            assert_eq!(status, expected_status, "op {op} script {script}");
            assert_eq!(
                headers.get("x-ctd-origin").map(String::as_str),
                Some("upstream"),
                "op {op}: terminal upstream response must carry x-ctd-origin: upstream"
            );
            assert_eq!(
                body, expected_body,
                "op {op} script {script}: body must be byte-true"
            );
        }
        other => panic!("unknown mode {other}"),
    }
}

/// UDS 端到端：起 serve()，手工 HTTP/1.1 over UnixStream 打内部端点，
/// 返回 (status, headers, body)。
async fn uds_round_trip(
    op: &str,
    script: &str,
) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config = codex_transportd::config::Config {
        codex_home: dir.path().join("home"),
        socket_path: dir.path().join("s.sock"),
        image_digest_override: None,
        build_proof_digest_override: None,
        socket_dir_expected_uid: None,
        socket_dir_expected_gid: None,
    };
    let mut server = tokio::spawn(codex_transportd::server::serve(config));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let socket_path = dir.path().join("s.sock");
    while !socket_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "server did not bind (serve finished: {})",
            server.is_finished()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    if server.is_finished() {
        // bind 之后、accept 之前失败：把 serve 的结果带出来。
        let result = (&mut server).await;
        panic!("serve exited right after bind: {result:?}");
    }

    // 先打 health/live 探针：隔离 server/客户端问题与 operation 问题。
    let (health_status, _, health_body) = uds_exchange(
        &socket_path,
        "GET /_internal/v1/health/live HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(health_status, 200, "health probe failed: {health_body:?}");

    let (method, path) = match op {
        "responses" => ("POST", "/_internal/v1/responses"),
        "compact" => ("POST", "/_internal/v1/responses/compact"),
        "models" => ("GET", "/_internal/v1/models"),
        other => panic!("unknown UDS operation {other}"),
    };
    let body = if method == "GET" {
        bytes::Bytes::new()
    } else {
        worker_body()
    };
    let if_none_match = if op == "models" && script == "304" {
        "x-ctd-if-none-match: \"models-etag\"\r\n"
    } else {
        ""
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\
         x-ctd-protocol-version: 1\r\n\
         x-ctd-request-id: req-non2xx-uds\r\n\
         x-ctd-account-key: acct\r\n\
         x-ctd-credential-revision: rev-1\r\n\
         x-ctd-access-token: test-token\r\n\
         x-ctd-proxy-kind: direct\r\n\
         x-ctd-session-id: 0198f2b0-0000-7000-8000-000000000001\r\n\
         x-ctd-thread-id: 0198f2b0-0000-7000-8000-000000000002\r\n\
         x-ctd-window-id: 0198f2b0-0000-7000-8000-000000000002:1\r\n\
         {if_none_match}\
         content-length: {}\r\n\r\n",
        body.len()
    );

    let (status, headers, body) = uds_exchange(&socket_path, &request, &body).await;

    server.abort();

    (status, headers, body)
}

/// 手工 HTTP/1.1 over UnixStream：写请求（含 body），读到 EOF，解析
/// status/headers/body（chunked 或 content-length）。诊断失败时携带原始
/// 字节。
async fn uds_exchange(
    socket_path: &std::path::Path,
    request_head: &str,
    request_body: &[u8],
) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let mut sock = tokio::net::UnixStream::connect(socket_path)
        .await
        .expect("connect uds");
    sock.write_all(request_head.as_bytes())
        .await
        .expect("write request head");
    sock.write_all(request_body)
        .await
        .expect("write request body");
    // 注意：tokio UnixStream 的 shutdown() 关闭双半边（非 TCP 式半关），
    // 会连接收侧一起关掉；请求以 content-length 分帧完整写出即可，
    // 响应读取终止由服务端 `connection: close` 保证。
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read response");

    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "response head terminator missing ({} bytes): {}",
                raw.len(),
                String::from_utf8_lossy(&raw)
            )
        });
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line: {status_line}"));
    let mut headers = std::collections::HashMap::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = value.parse().ok();
        }
        if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        headers.insert(name, value);
    }
    let raw_body = &raw[head_end + 4..];
    let body = if chunked {
        decode_chunked(raw_body)
    } else if let Some(len) = content_length {
        raw_body[..len].to_vec()
    } else {
        raw_body.to_vec()
    };
    (status, headers, body)
}

/// 最小 chunked 解码（hyper 对无长度流式 body 使用 chunked）。
fn decode_chunked(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let Some(line_end) = raw[pos..].windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size = usize::from_str_radix(
            std::str::from_utf8(&raw[pos..pos + line_end])
                .expect("chunk size utf8")
                .split(';')
                .next()
                .expect("chunk size")
                .trim(),
            16,
        )
        .expect("chunk size hex");
        pos += line_end + 2;
        if size == 0 {
            break;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
    out
}
