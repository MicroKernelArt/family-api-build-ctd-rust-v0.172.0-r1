//! UDS connections are bounded before Hyper parses a complete request.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn uds_connections_are_bounded_before_request_admission() {
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

    let mut partial_clients = Vec::with_capacity(codex_transportd::limits::MAX_UDS_CONNECTIONS);
    for _ in 0..codex_transportd::limits::MAX_UDS_CONNECTIONS {
        let mut stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect partial-header client");
        stream
            .write_all(b"GET /_internal/v1/health/live HTTP/1.1\r\nhost: localhost\r\nx-partial:")
            .await
            .expect("write partial request headers");
        partial_clients.push(stream);
        tokio::task::yield_now().await;
    }

    let health_socket = socket_path.clone();
    let mut health = tokio::spawn(async move {
        let mut stream = tokio::net::UnixStream::connect(health_socket)
            .await
            .expect("connect health client");
        stream
            .write_all(
                b"GET /_internal/v1/health/live HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
            )
            .await
            .expect("write health request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read health response");
        response
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut health)
            .await
            .is_err(),
        "health request must wait while all UDS connection slots are occupied"
    );

    partial_clients.pop();
    let response = tokio::time::timeout(Duration::from_secs(2), health)
        .await
        .expect("health request should progress after one connection closes")
        .expect("health task");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "unexpected health response: {}",
        String::from_utf8_lossy(&response)
    );

    server.abort();
}
