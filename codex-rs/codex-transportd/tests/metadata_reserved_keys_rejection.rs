//! R-METADATA-001（replay_0831 R6）端到端零上游 I/O：携带核心保留键
//! `x-codex-parent-thread-id` 的 `client_metadata` 必须在出站构造前被
//! 拒绝（`reserved_metadata_conflict`），mock 上游零连接。
//!
//! 隔离：`CTD_TEST_UPSTREAM_BASE_URL` 是进程级 override（仅 test-upstream
//! feature 生效）；父进程持有 mock 上游并统计连接数，子进程只拿 env。

#![cfg(feature = "test-upstream")]

use std::io::{Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

const WORKER_TEST: &str = "metadata_reserved_keys_worker";

struct MockUpstream {
    addr: std::net::SocketAddr,
    connections: Arc<AtomicUsize>,
}

fn spawn_mock() -> MockUpstream {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let conns = Arc::clone(&connections);
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { break };
            conns.fetch_add(1, Ordering::SeqCst);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut chunk).expect("read request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let resp =
                b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\nconnection: close\r\n\r\nretry: 0\n\n";
            sock.write_all(resp).expect("write response");
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    });
    MockUpstream { addr, connections }
}

#[test]
fn reserved_parent_thread_key_never_reaches_upstream() {
    let mock = spawn_mock();
    let exe = std::env::current_exe().expect("current test binary");
    let out = Command::new(exe)
        .args([WORKER_TEST, "--exact", "--test-threads=1"])
        .env(
            "CTD_TEST_UPSTREAM_BASE_URL",
            format!("http://{}", mock.addr),
        )
        .env("CTD_RESERVED_KEY_CHILD", "1")
        .output()
        .expect("spawn worker child");
    assert!(
        out.status.success(),
        "worker failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        mock.connections.load(Ordering::SeqCst),
        0,
        "reserved-key rejection must not touch upstream"
    );
}

#[tokio::test]
async fn metadata_reserved_keys_worker() {
    if std::env::var("CTD_RESERVED_KEY_CHILD").is_err() {
        // 主套件直跑占位：真实逻辑仅由父测试 spawn 的 --exact 子进程执行。
        return;
    }
    use codex_transportd::contract::RequestContext;

    let base = std::env::var("CTD_TEST_UPSTREAM_BASE_URL").unwrap_or_default();
    assert_eq!(
        std::env::var("CTD_RESERVED_KEY_CHILD").as_deref(),
        Ok("1"),
        "worker must run as the spawned child, never in-process"
    );
    let parsed = url::Url::parse(&base).expect("base url parses");
    assert_eq!(parsed.scheme(), "http", "mock is plain loopback http");
    assert!(
        matches!(
            parsed.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
        ),
        "mock base must be loopback, got {base}"
    );

    let ctx = RequestContext {
        request_id: "req-reserved-key".to_string(),
        account_key: "acct".to_string(),
        credential_revision: "rev-1".to_string(),
        access_token: codex_transportd::redaction::SecretString::new("test-token"),
        chatgpt_account_id: None,
        operation: codex_transportd::contract::operation::Operation::Responses,
        proxy_kind: codex_transportd::contract::ProxyKind::Direct,
        proxy_url: None,
        session: Some(codex_transportd::contract::headers::SessionSnapshot {
            session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
            thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
            window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        }),
        purpose: codex_transportd::contract::RequestPurpose::Request,
        ..Default::default()
    };
    let pool = codex_transportd::transport::client_pool::ClientPool::new(Arc::new(
        codex_transportd::observability::Metrics::default(),
    ));
    let body: bytes::Bytes = serde_json::json!({
        "model": "gpt-5.2",
        "client_metadata": {"x-codex-parent-thread-id": "attacker"},
    })
    .to_string()
    .into_bytes()
    .into();

    let result = codex_transportd::transport::responses::execute_responses_raw(
        &pool,
        "installation-1",
        &ctx,
        body,
    )
    .await;
    let err = match result {
        Ok(raw) => panic!(
            "reserved core key must be rejected before upstream I/O, got status {}",
            raw.status
        ),
        Err(err) => err,
    };
    assert_eq!(err.code, "reserved_metadata_conflict", "err: {err:?}");
}
