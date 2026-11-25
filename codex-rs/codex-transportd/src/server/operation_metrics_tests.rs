//! R-OBS-001（replay_0831 R6）：真实 retry 计数进入有基数上限的指标。
//!
//! 断言 server 层把 raw 适配返回的真实 attempt 次数回填
//! `retries_total{operation,outcome}`：success-after-retry 与 exhausted
//! 精确计数、无 retry 零增量、无双计。
//!
//! 隔离：`CTD_TEST_UPSTREAM_BASE_URL` 是进程级 override（仅 test-upstream
//! feature 生效）。父进程持有 mock 上游；子进程以自建 `ServerState` 直接
//! 驱动 `execute_operation`（ owns 自己的 `Metrics`），env 只注入子进程。

#![cfg(feature = "test-upstream")]

use std::io::{Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// worker 测试名（子进程 --exact 过滤；crate 内测试需带完整模块路径）。
const WORKER_TEST: &str = "server::operation_metrics_tests::operation_metrics_worker";
/// 场景 env：`<responses|compact>|<script>`。
const SCENARIO_ENV: &str = "CTD_OBS_SCENARIO";
const CHILD_ENV: &str = "CTD_OBS_CHILD";

const BODY_500: &[u8] = b"{\"error\":\"boom\"}";
const BODY_SSE_OK: &[u8] = b"retry: 0\n\n";

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
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).expect("write head");
            sock.write_all(body).expect("write body");
            let _ = sock.flush();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    });
    MockUpstream { addr, connections }
}

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

#[test]
fn retry_metrics_exact_counts_success_after_retry() {
    let mock = spawn_mock(vec![(500, BODY_500), (200, BODY_SSE_OK)]);
    run_child("responses|500_200", &mock);
    assert_eq!(mock.connections.load(Ordering::SeqCst), 2);
}

#[test]
fn retry_metrics_exact_counts_exhausted() {
    let mock = spawn_mock(vec![
        (500, BODY_500),
        (500, BODY_500),
        (500, BODY_500),
        (500, BODY_500),
    ]);
    run_child("responses|500x4", &mock);
    // 官方循环 0..=max_attempts → 总尝试 5（4 次 retry）。
    assert_eq!(mock.connections.load(Ordering::SeqCst), 5);
}

#[test]
fn retry_metrics_exact_counts_no_retry_and_compact() {
    // 403：无 retry → retries_total 零增量。
    let mock = spawn_mock(vec![(403, b"{\"error\":\"denied\"}".as_slice())]);
    run_child("responses|403", &mock);
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1);
    // compact：success-after-retry 同样按 operation 分桶。
    let mock = spawn_mock(vec![(500, BODY_500), (200, BODY_SSE_OK)]);
    run_child("compact|500_200", &mock);
    assert_eq!(mock.connections.load(Ordering::SeqCst), 2);
}

fn worker_scenario() -> (String, String) {
    let raw = std::env::var(SCENARIO_ENV).unwrap_or_default();
    let mut parts = raw.split('|');
    let op = parts.next().unwrap_or_default().to_string();
    let script = parts.next().unwrap_or_default().to_string();
    assert_eq!(
        std::env::var(CHILD_ENV).as_deref(),
        Ok("1"),
        "worker must run as the spawned child, never in-process"
    );
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
    (op, script)
}

fn expected_retries_snapshot(op: &str, script: &str) -> std::collections::BTreeMap<String, u64> {
    let label = if op == "compact" {
        "responses_compact"
    } else {
        "responses"
    };
    match script {
        "500_200" => {
            std::collections::BTreeMap::from([(format!("{label}|success_after_retry"), 1)])
        }
        "500x4" => std::collections::BTreeMap::from([(format!("{label}|retry_exhausted"), 4)]),
        "403" => std::collections::BTreeMap::new(),
        other => panic!("unknown script {other}"),
    }
}

#[tokio::test]
async fn operation_metrics_worker() {
    // R-OBS-001 child-spawn guard：worker 的真实逻辑只由父测试
    // retry_metrics_exact_counts_* spawn 的子进程执行——run_child() 以
    // `--exact --test-threads=1` 跑本测试并注入 CHILD_ENV=1 /
    // SCENARIO_ENV / CTD_TEST_UPSTREAM_BASE_URL。主套件直接执行本测试时
    // 这些 env 不存在（无 mock 上游、无场景），在此占位 return；
    // 真实断言仍只在子进程内执行，worker_scenario() 不放宽任何断言。
    if std::env::var_os(CHILD_ENV).is_none() && std::env::var_os(SCENARIO_ENV).is_none() {
        return;
    }
    let (op, script) = worker_scenario();

    let dir = tempfile::TempDir::new().expect("tempdir");
    let metrics = Arc::new(crate::observability::Metrics::default());
    let state = Arc::new(crate::server::ServerState {
        config: crate::config::Config {
            codex_home: dir.path().join("home"),
            socket_path: dir.path().join("unused.sock"),
            image_digest_override: None,
            build_proof_digest_override: None,
            socket_dir_expected_uid: None,
            socket_dir_expected_gid: None,
        },
        installation_id: "0198f2b0-0000-7000-8000-000000000009".to_string(),
        metrics: Arc::clone(&metrics),
        pool: Arc::new(crate::transport::client_pool::ClientPool::new(Arc::clone(
            &metrics,
        ))),
        inflight: Arc::new(tokio::sync::Semaphore::new(crate::limits::MAX_INFLIGHT)),
        ws_slots: Arc::new(tokio::sync::Semaphore::new(
            crate::limits::MAX_WS_CONNECTIONS,
        )),
    });

    let operation = if op == "compact" {
        crate::contract::operation::Operation::ResponsesCompact
    } else {
        crate::contract::operation::Operation::Responses
    };
    let ctx = crate::contract::RequestContext {
        request_id: "req-obs".to_string(),
        account_key: "acct".to_string(),
        credential_revision: "rev-1".to_string(),
        access_token: crate::redaction::SecretString::new("test-token"),
        chatgpt_account_id: None,
        operation,
        proxy_kind: crate::contract::ProxyKind::Direct,
        proxy_url: None,
        session: Some(crate::contract::headers::SessionSnapshot {
            session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
            thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
            window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        }),
        purpose: crate::contract::RequestPurpose::Request,
        ..Default::default()
    };
    let body: bytes::Bytes = serde_json::json!({
        "model": "gpt-5.2-obs",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "obs-marker"}],
        }],
    })
    .to_string()
    .into_bytes()
    .into();

    let response = crate::server::execute_operation(&state, &ctx, body)
        .await
        .unwrap_or_else(|err| panic!("operation must execute: {err:?}"));

    let expected_status = match script.as_str() {
        "500_200" => 200,
        "500x4" => 500,
        "403" => 403,
        other => panic!("unknown script {other}"),
    };
    assert_eq!(
        response.status().as_u16(),
        expected_status,
        "op {op} script {script}"
    );
    assert_eq!(
        response
            .headers()
            .get("x-ctd-origin")
            .map(|v| v.to_str().expect("header value")),
        Some("upstream"),
        "terminal upstream response must carry x-ctd-origin: upstream"
    );

    // R-OBS-001 核心：整表精确断言（无 retry → 空表；有 retry → 恰好一个
    // outcome 桶、恰好 retries 次数）。
    let snapshot = metrics.retries_total.snapshot();
    assert_eq!(
        snapshot,
        expected_retries_snapshot(op.as_str(), script.as_str()),
        "op {op} script {script}: retries_total must be exact and single-counted"
    );
}
