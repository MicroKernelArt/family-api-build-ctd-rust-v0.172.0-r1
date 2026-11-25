//! Deadline-less operations use an operation-specific server budget.

#![cfg(feature = "test-upstream")]

use std::process::Command;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

const CHILD_ENV: &str = "CTD_DEFAULT_DEADLINE_CHILD";
const WORKER_TEST: &str = "deadline_less_compact_worker";

#[test]
fn deadline_less_compact_allows_adapter_timeout_budget() {
    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .args([WORKER_TEST, "--exact", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn worker child");

    assert!(
        output.status.success(),
        "deadline worker failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
async fn deadline_less_compact_worker() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
        return;
    }

    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let upstream_addr = upstream.local_addr().expect("mock upstream address");
    // This exact-test child is single-threaded and sets the override before
    // spawning any server or client task; no concurrent environment access exists.
    unsafe {
        std::env::set_var(
            "CTD_TEST_UPSTREAM_BASE_URL",
            format!("http://{upstream_addr}"),
        );
    }

    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let mock = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.expect("accept upstream request");
        read_http_request(&mut stream).await;
        let response_delay = tokio::time::sleep(Duration::from_secs(400));
        let _ = request_seen_tx.send(());
        response_delay.await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .await
            .expect("write delayed upstream response");
    });

    let dir = tempfile::TempDir::new().expect("tempdir");
    let socket_path = dir.path().join("transportd.sock");
    let config = codex_transportd::config::Config {
        codex_home: dir.path().join("home"),
        socket_path: socket_path.clone(),
        image_digest_override: None,
        build_proof_digest_override: None,
        socket_dir_expected_uid: None,
        socket_dir_expected_gid: None,
    };
    let server = tokio::spawn(codex_transportd::server::serve(config));
    while !socket_path.exists() {
        assert!(!server.is_finished(), "server exited before binding");
        tokio::task::yield_now().await;
    }

    let body = br#"{"model":"gpt-test","input":[]}"#;
    let request = format!(
        "POST /_internal/v1/responses/compact HTTP/1.1\r\n\
         host: localhost\r\n\
         connection: close\r\n\
         x-ctd-protocol-version: 1\r\n\
         x-ctd-request-id: req-default-deadline\r\n\
         x-ctd-account-key: acct\r\n\
         x-ctd-credential-revision: rev-1\r\n\
         x-ctd-access-token: test-token\r\n\
         x-ctd-proxy-kind: direct\r\n\
         x-ctd-session-id: 0198f2b0-0000-7000-8000-000000000001\r\n\
         x-ctd-thread-id: 0198f2b0-0000-7000-8000-000000000002\r\n\
         x-ctd-window-id: 0198f2b0-0000-7000-8000-000000000002:1\r\n\
         content-length: {}\r\n\r\n",
        body.len()
    );
    let client_socket = socket_path.clone();
    let client = tokio::spawn(async move {
        let mut stream = tokio::net::UnixStream::connect(client_socket)
            .await
            .expect("connect transportd");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request head");
        stream.write_all(body).await.expect("write request body");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        response
    });

    request_seen_rx
        .await
        .expect("compact reached mock upstream");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(400)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();

    let response = tokio::time::timeout(Duration::from_secs(1), client)
        .await
        .expect("deadline-less compact response should complete")
        .expect("client task");
    let response = String::from_utf8(response).expect("HTTP response is UTF-8");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected delayed upstream 2xx, got {response}"
    );

    mock.await.expect("mock task");
    server.abort();
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).await.expect("read request head");
        assert!(read > 0, "upstream request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]);
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < head_end + content_length {
        let read = stream.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "upstream request ended before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
}
