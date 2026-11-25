//! 出站契约（真实出站构造 → mock 上游观测）：
//!
//! 1. **T3 负向（红线，capture-r1 F3）**：带 `x-ctd-user-id` 的入站 context 走
//!    真实出站构造（`execute_responses_raw` → 官方 client → mock 上游），
//!    mock 上游观测到的请求头里任何 `x-ctd-*` 一律缺席。
//! 2. **v1 出站保真（docs/22 §6 项 ①/⑤/②）**：头名序列 == 官方 0.159.2 的
//!    18 头构造序；`content-encoding: zstd` + 真 zstd 体；body `client_metadata`
//!    键集合 == 官方 9 键。
//!
//! 隔离：`CTD_TEST_UPSTREAM_BASE_URL` 是进程级 override（仅 test-upstream
//! feature 生效，TLS-CTD-001 §15.2 既有测试 harness）。为不污染同进程的
//! 其他测试（如官方 base URL 断言），真实请求在自孵化子进程中执行：
//! 父进程用 std::TcpListener 起 mock，`current_exe()` 重入测试二进制并只
//! 跑 worker 测试（--exact），env 只注入子进程——父进程零 env 变更。

#![cfg(feature = "test-upstream")]

use std::io::{Read, Write};
use std::process::Command;

use bytes::Bytes;
use codex_transportd::contract::RequestContext;
use codex_transportd::contract::headers::SessionSnapshot;
use codex_transportd::observability::Metrics;
use codex_transportd::redaction::SecretString;
use codex_transportd::transport::client_pool::ClientPool;
use codex_transportd::transport::responses::execute_responses_raw;
use futures::StreamExt;

/// worker 测试名（子进程 --exact 过滤）。
const WORKER_TEST: &str = "capture_t3_mock_upstream_worker";

#[test]
fn mock_upstream_never_observes_x_ctd_headers() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(&listener));

    let exe = std::env::current_exe().expect("current test binary");
    let out = Command::new(exe)
        .args([WORKER_TEST, "--exact", "--test-threads=1"])
        .env("CTD_TEST_UPSTREAM_BASE_URL", format!("http://{addr}"))
        .env("CTD_T3_CHILD", "1")
        .output()
        .expect("spawn worker child");
    assert!(
        out.status.success(),
        "worker failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let (head, body) = server.join().expect("mock server thread");
    let request_line = head.split("\r\n").next().unwrap_or_default();
    assert!(
        request_line.starts_with("POST /responses"),
        "unexpected request line: {request_line}"
    );
    let mut saw_session = false;
    let mut saw_bearer = false;
    for line in head.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let (name, _) = line.split_once(':').unwrap_or((line, ""));
        let name = name.trim().to_ascii_lowercase();
        // 红线（G6/T3）：任何 x-ctd-* 内部 header 都不得出现在真实上游请求。
        assert!(
            !name.starts_with("x-ctd-"),
            "internal header namespace leaked to the real outbound request: {name}"
        );
        if name == "session-id" {
            saw_session = true;
        }
        if name == "authorization" {
            saw_bearer = true;
        }
    }
    assert!(
        saw_session,
        "outbound must carry official session headers: {head}"
    );
    assert!(
        saw_bearer,
        "outbound must carry the official bearer auth path: {head}"
    );
    // ⑤ 官方出站 body 是 zstd 压缩体（`docs/21 §5.1`）：mock 上游看到的必须是
    // 真压缩字节 + 与之一致的 `content-encoding`，解压后才是 ctd 交付的 JSON。
    assert!(
        head.to_ascii_lowercase()
            .contains("\r\ncontent-encoding: zstd\r\n"),
        "outbound must declare the official zstd encoding: {head}"
    );
    assert_eq!(
        &body[..4],
        &[0x28, 0xb5, 0x2f, 0xfd],
        "outbound body must be a zstd frame"
    );
    let plain = zstd::stream::decode_all(body.as_slice()).expect("outbound body must decompress");
    assert!(
        plain
            .windows(b"capture-t3-marker".len())
            .any(|w| w == b"capture-t3-marker"),
        "request body must reach the mock upstream byte-true"
    );
}

/// 最小 HTTP/1.1 mock 上游：读取一个请求（head + content-length body），
/// 回一个最小 SSE 形态 200，返回 (head, body) 供断言。
fn serve_one(listener: &std::net::TcpListener) -> (String, Vec<u8>) {
    let (mut sock, _) = listener.accept().expect("accept");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).expect("read request head");
        assert!(n > 0, "eof before request head completed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        assert!(buf.len() < 64 * 1024, "request head exceeds bound");
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let content_length = head
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    assert!(
        content_length <= 8 * 1024 * 1024,
        "content-length exceeds bound"
    );
    while buf.len() < head_end + content_length {
        let n = sock.read(&mut chunk).expect("read request body");
        assert!(n > 0, "eof before request body completed");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[head_end..head_end + content_length].to_vec();
    let resp = b"retry: 0\n\n";
    sock.write_all(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
            resp.len()
        )
        .as_bytes(),
    )
    .expect("write response head");
    sock.write_all(resp).expect("write response body");
    (head, body)
}

/// 子进程内执行真实出站构造；env 由父进程注入（本测试自身不变更 env）。
#[tokio::test]
async fn capture_t3_mock_upstream_worker() {
    if std::env::var("CTD_T3_CHILD").is_err() {
        // 主套件直跑占位：真实逻辑仅由父测试 spawn 的 --exact 子进程执行。
        return;
    }
    let base = std::env::var("CTD_TEST_UPSTREAM_BASE_URL").unwrap_or_default();
    assert!(
        !base.is_empty(),
        "run via mock_upstream_never_observes_x_ctd_headers (needs CTD_TEST_UPSTREAM_BASE_URL)"
    );
    assert_eq!(
        std::env::var("CTD_T3_CHILD").as_deref(),
        Ok("1"),
        "worker must run as the spawned child, never in-process"
    );
    // 双保险：override 只允许指向 loopback，绝无打到官方端点的可能。
    let parsed = url::Url::parse(&base).expect("base url parses");
    assert_eq!(parsed.scheme(), "http", "mock is plain loopback http");
    assert!(
        matches!(
            parsed.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
        ),
        "mock base must be loopback, got {}",
        base
    );

    let ctx = RequestContext {
        request_id: "req_capture_t3".to_string(),
        account_key: "acct".to_string(),
        credential_revision: "rev-1".to_string(),
        access_token: SecretString::new("test-token"),
        chatgpt_account_id: Some(SecretString::new("acc-1")),
        proxy_kind: codex_transportd::contract::ProxyKind::Direct,
        session: Some(SessionSnapshot {
            session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
            thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
            window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        }),
        // T3 断言对象：capture 白名单身份进入 context。
        user_id: Some("3".to_string()),
        ..Default::default()
    };
    let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));
    let body: Bytes = serde_json::json!({
        "model": "gpt-5.2-capture-t3",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "capture-t3-marker"}],
        }],
    })
    .to_string()
    .into_bytes()
    .into();

    let resp = execute_responses_raw(&pool, "installation-1", &ctx, body)
        .await
        .expect("real outbound construction must execute");
    assert_eq!(resp.status, 200, "mock upstream answered 200");
    let mut received = Vec::new();
    let mut stream = resp.body;
    while let Some(chunk) = stream.next().await {
        received.extend_from_slice(&chunk.expect("mock stream chunk"));
    }
    assert_eq!(received, b"retry: 0\n\n", "mock body forwards byte-true");
}

// ---------------------------------------------------------------------------
// v1 出站保真（docs/22 §6 项 ①/⑤/②）
// ---------------------------------------------------------------------------

/// worker 测试名（子进程 --exact 过滤）。
const V1_FIDELITY_WORKER: &str = "v1_outbound_fidelity_worker";
/// 子进程标记（防止主套件直跑真实出站）。
const V1_FIDELITY_CHILD: &str = "CTD_V1_FIDELITY_CHILD";

/// 官方 0.159.2 `POST /backend-api/codex/responses` 的头名序列
/// （`tools/ctd-parity/baseline/official-1592.json:headers_main_post.rows`，
/// 5 组采集 × 90 条 POST 零变异 ⇒ 强不变量）。
const OFFICIAL_POST_HEADER_SEQUENCE: [&str; 18] = [
    "version",
    "x-codex-beta-features",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-openai-internal-codex-responses-lite",
    "x-codex-routing-hint",
    "x-client-request-id",
    "session-id",
    "thread-id",
    "accept",
    "content-encoding",
    "content-type",
    "authorization",
    "chatgpt-account-id",
    "originator",
    "user-agent",
    "host",
    "content-length",
];

/// 官方 `body.client_metadata` 键集合（同一基准 `body.client_metadata_keys`）。
const OFFICIAL_CLIENT_METADATA_KEYS: [&str; 9] = [
    "guardian_credits_requested",
    "mcp_attribution",
    "root_turn_id",
    "session_id",
    "thread_id",
    "turn_id",
    "x-codex-installation-id",
    "x-codex-turn-metadata",
    "x-codex-window-id",
];

#[test]
fn v1_outbound_matches_official_1592_wire_shape() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(&listener));

    let exe = std::env::current_exe().expect("current test binary");
    let out = Command::new(exe)
        .args([V1_FIDELITY_WORKER, "--exact", "--test-threads=1"])
        .env("CTD_TEST_UPSTREAM_BASE_URL", format!("http://{addr}"))
        .env(V1_FIDELITY_CHILD, "1")
        .output()
        .expect("spawn worker child");
    assert!(
        out.status.success(),
        "worker failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let (head, body) = server.join().expect("mock server thread");
    let mut lines = head.split("\r\n");
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .starts_with("POST /responses"),
        "unexpected request line: {head}"
    );
    let header_pairs: Vec<(String, String)> = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let names: Vec<String> = header_pairs.iter().map(|(name, _)| name.clone()).collect();
    // ① 头名序列（含顺序）必须逐位等于官方构造序。
    assert_eq!(names, OFFICIAL_POST_HEADER_SEQUENCE, "wire head:\n{head}");
    let values: std::collections::HashMap<String, String> = header_pairs.into_iter().collect();
    // ⑥ 身份：`version` 与 UA 版本段同源；UA 取官方 TUI 形态（尾后缀按
    // originator 补齐），平台/终端段保持承载值。
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(values["version"], version);
    assert_eq!(values["originator"], "codex-tui");
    assert_eq!(
        values["user-agent"],
        format!(
            "codex-tui/{version} (Ubuntu 22.4.0; x86_64) xterm-256color (codex-tui; {version})"
        )
    );
    // `session-id` / `thread-id` 由会话快照派生；承载通道未提供时不得缺失。
    assert_eq!(values["session-id"], "0198f2b0-0000-7000-8000-000000000001");
    assert_eq!(values["thread-id"], "0198f2b0-0000-7000-8000-000000000002");

    // ⑤ 真压缩 + 与之一致的头 + content-length = 压缩后字节数。
    assert_eq!(&body[..4], &[0x28, 0xb5, 0x2f, 0xfd], "body must be zstd");
    let declared: usize = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .expect("content-length present");
    assert_eq!(declared, body.len(), "content-length must be the zstd size");

    // ② body `client_metadata` 键集合 == 官方 9 键。
    let plain = zstd::stream::decode_all(body.as_slice()).expect("zstd body");
    let request: serde_json::Value = serde_json::from_slice(&plain).expect("json body");
    let metadata = request["client_metadata"]
        .as_object()
        .expect("client_metadata object");
    let mut keys: Vec<&str> = metadata.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = OFFICIAL_CLIENT_METADATA_KEYS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    // 派生键的真值（官方 exec 0.159.2 基准值）。
    assert_eq!(metadata["guardian_credits_requested"], "true");
    assert_eq!(metadata["mcp_attribution"], r#"{"status":"none"}"#);
    assert_eq!(
        metadata["turn_id"],
        metadata["x-codex-turn-metadata"]
            .as_str()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|turn| turn["turn_id"].as_str().map(str::to_owned))
            .expect("turn_id present in turn metadata")
    );
    // I6/I7：body 与请求头同源。
    let header_turn_metadata = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("x-codex-turn-metadata"))
        .map(|(_, value)| value.trim().to_string())
        .expect("x-codex-turn-metadata header");
    assert_eq!(
        metadata["x-codex-turn-metadata"].as_str().unwrap(),
        header_turn_metadata
    );
    assert_eq!(metadata["root_turn_id"], metadata["turn_id"]);
}

/// 子进程内执行真实出站构造（生产形态的承载集合）；env 由父进程注入。
#[tokio::test]
async fn v1_outbound_fidelity_worker() {
    if std::env::var(V1_FIDELITY_CHILD).is_err() {
        // 主套件直跑占位：真实逻辑仅由父测试 spawn 的 --exact 子进程执行。
        return;
    }
    let base = std::env::var("CTD_TEST_UPSTREAM_BASE_URL").unwrap_or_default();
    assert!(
        !base.is_empty(),
        "run via v1_outbound_matches_official_1592_wire_shape"
    );
    assert_eq!(std::env::var(V1_FIDELITY_CHILD).as_deref(), Ok("1"));

    // 生产形态承载集合：Go 白名单里 v1 responses 实际会出现的头。
    let carried = std::collections::BTreeMap::from([
        (
            "x-codex-beta-features".to_string(),
            "remote_compaction_v2".to_string(),
        ),
        (
            "x-codex-window-id".to_string(),
            "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        ),
        (
            "x-codex-turn-metadata".to_string(),
            r#"{"request_kind":"turn","session_id":"0198f2b0-0000-7000-8000-000000000001","thread_id":"0198f2b0-0000-7000-8000-000000000002","turn_id":"0198f2b0-0000-7000-8000-000000000003","root_turn_id":"0198f2b0-0000-7000-8000-000000000003","window_id":"0198f2b0-0000-7000-8000-000000000002:1"}"#
                .to_string(),
        ),
        (
            "x-openai-internal-codex-responses-lite".to_string(),
            "true".to_string(),
        ),
        (
            "x-codex-routing-hint".to_string(),
            "model=gpt-5.6-sol".to_string(),
        ),
        (
            "x-client-request-id".to_string(),
            "0198f2b0-0000-7000-8000-000000000001".to_string(),
        ),
        ("originator".to_string(), "codex-tui".to_string()),
        (
            "user-agent".to_string(),
            "codex-tui/0.159.0 (Ubuntu 22.4.0; x86_64) xterm-256color".to_string(),
        ),
    ]);
    let ctx = RequestContext {
        request_id: "req_v1_fidelity".to_string(),
        account_key: "acct".to_string(),
        credential_revision: "rev-1".to_string(),
        access_token: SecretString::new("test-token"),
        chatgpt_account_id: Some(SecretString::new("acc-1")),
        proxy_kind: codex_transportd::contract::ProxyKind::Direct,
        session: Some(SessionSnapshot {
            session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
            thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
            window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        }),
        carried_headers: carried,
        ..Default::default()
    };
    let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));
    let body: Bytes = serde_json::json!({
        "model": "gpt-5.6-sol",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "v1-fidelity-marker"}],
        }],
        "store": false,
        "stream": true,
    })
    .to_string()
    .into_bytes()
    .into();

    let resp = execute_responses_raw(&pool, "installation-1", &ctx, body)
        .await
        .expect("real outbound construction must execute");
    assert_eq!(resp.status, 200, "mock upstream answered 200");
    let mut stream = resp.body;
    while let Some(chunk) = stream.next().await {
        chunk.expect("mock stream chunk");
    }
}

// ---------------------------------------------------------------------------
// v3 出站保真（hookpoint=v3：Go 交付最终字节 + 完整出站头集）
// ---------------------------------------------------------------------------

/// worker 测试名（子进程 --exact 过滤）。
const V3_FIDELITY_WORKER: &str = "v3_outbound_fidelity_worker";
/// 子进程标记（防止主套件直跑真实出站）。
const V3_FIDELITY_CHILD: &str = "CTD_V3_FIDELITY_CHILD";

/// v3 交付的完整出站头集（Go 侧 `codexV3HeaderNames` / Rust 侧
/// `V3_HEADER_ALLOWLIST` 的白名单全集，值为官方 159.2 exec 形态）。
fn v3_delivered_outbound_headers() -> std::collections::BTreeMap<String, String> {
    [
        ("version", "0.159.2"),
        ("x-codex-beta-features", "remote_compaction_v2"),
        (
            "x-codex-window-id",
            "0198f2b0-0000-7000-8000-000000000002:1",
        ),
        (
            "x-codex-turn-metadata",
            r#"{"request_kind":"turn","session_id":"0198f2b0-0000-7000-8000-000000000001","thread_id":"0198f2b0-0000-7000-8000-000000000002","turn_id":"0198f2b0-0000-7000-8000-000000000003","root_turn_id":"0198f2b0-0000-7000-8000-000000000003","window_id":"0198f2b0-0000-7000-8000-000000000002:1","window_number":1}"#,
        ),
        ("x-openai-internal-codex-responses-lite", "true"),
        ("x-codex-routing-hint", "model=gpt-5.6-sol"),
        (
            "x-client-request-id",
            "0198f2b0-0000-7000-8000-000000000001",
        ),
        ("session-id", "0198f2b0-0000-7000-8000-000000000001"),
        ("thread-id", "0198f2b0-0000-7000-8000-000000000002"),
        ("accept", "text/event-stream"),
        ("content-encoding", "zstd"),
        ("content-type", "application/json"),
        ("originator", "codex_exec"),
        (
            "user-agent",
            "codex_exec/0.159.2 (Mac OS 15.5.0; arm64) WarpTerminal/v1 (codex_exec; 0.159.2)",
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect()
}

/// ★ v3 出站保真（真实出站构造 → mock 上游观测）：头名序列必须逐位等于官方
/// 159.2 的 18 头构造序，body 必须是 Go 交付的 zstd 字节**逐字**（ctd 不解析、
/// 不重压、不改头），`content-length` 必须等于交付字节数。
///
/// 这条断言覆盖单元测试无法覆盖的那一段：seq 由
/// `extra_headers` → `auth.apply_auth` → 尾段注入 → reqwest 默认头 →
/// hyper 传输头 的真实插入序决定，即"Auth 之后才是 originator/user-agent"
/// 这个**只有 ctd 知道**的切分点是否真的落到线上。
#[test]
fn v3_outbound_matches_official_1592_wire_shape() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(&listener));

    let exe = std::env::current_exe().expect("current test binary");
    let out = Command::new(exe)
        .args([V3_FIDELITY_WORKER, "--exact", "--test-threads=1"])
        .env("CTD_TEST_UPSTREAM_BASE_URL", format!("http://{addr}"))
        .env(V3_FIDELITY_CHILD, "1")
        .output()
        .expect("spawn worker child");
    assert!(
        out.status.success(),
        "worker failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let (head, body) = server.join().expect("mock server thread");
    let mut lines = head.split("\r\n");
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .starts_with("POST /responses"),
        "unexpected request line: {head}"
    );
    let names: Vec<String> = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_ascii_lowercase())
        .collect();
    // ★ 头序（含顺序）逐位等于官方构造序——v3 交付前是字母序（15 头）。
    assert_eq!(names, OFFICIAL_POST_HEADER_SEQUENCE, "wire head:\n{head}");

    // content-length = 交付的压缩体字节数（传输栈按最终字节生成）。
    let declared: usize = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .expect("content-length present");
    assert_eq!(
        declared,
        body.len(),
        "content-length must be the delivered size"
    );
    assert_eq!(
        &body[..4],
        &[0x28, 0xb5, 0x2f, 0xfd],
        "delivered body must be a zstd frame"
    );

    // ★ body 逐字透传：ctd 不解析不改写 ⇒ 解压后即 Go 交付的明文。
    let plain = zstd::stream::decode_all(body.as_slice()).expect("zstd body");
    let request: serde_json::Value = serde_json::from_slice(&plain).expect("json body");
    assert_eq!(request["model"], "gpt-5.6-sol");
    assert_eq!(
        request["client_metadata"]["guardian_credits_requested"],
        "true"
    );
    assert_eq!(
        request["client_metadata"]["mcp_attribution"],
        r#"{"status":"none"}"#
    );
    // I6/I7：body 与请求头同源。
    let header_turn_metadata = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("x-codex-turn-metadata"))
        .map(|(_, value)| value.trim().to_string())
        .expect("x-codex-turn-metadata header");
    assert_eq!(
        request["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap(),
        header_turn_metadata
    );
}

/// 子进程内执行 v3 真实出站（Go 交付形态：最终字节 + 完整出站头集）。
#[tokio::test]
async fn v3_outbound_fidelity_worker() {
    if std::env::var(V3_FIDELITY_CHILD).is_err() {
        return;
    }
    let base = std::env::var("CTD_TEST_UPSTREAM_BASE_URL").unwrap_or_default();
    assert!(
        !base.is_empty(),
        "run via v3_outbound_matches_official_1592_wire_shape"
    );

    let ctx = RequestContext {
        request_id: "req_v3_fidelity".to_string(),
        account_key: "acct".to_string(),
        credential_revision: "rev-1".to_string(),
        access_token: SecretString::new("test-token"),
        chatgpt_account_id: Some(SecretString::new("acc-1")),
        proxy_kind: codex_transportd::contract::ProxyKind::Direct,
        session: Some(SessionSnapshot {
            session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
            thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
            window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        }),
        outbound_headers: v3_delivered_outbound_headers(),
        hookpoint: codex_transportd::contract::Hookpoint::V3,
        ..Default::default()
    };
    let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));

    // Go 交付的最终字节：明文（含官方 client_metadata 9 键）→ zstd（模拟
    // `compressCodexV3Body`：同一权威在同一时刻产出字节与 content-encoding）。
    let plain = serde_json::json!({
        "model": "gpt-5.6-sol",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "v3-fidelity-marker"}],
        }],
        "store": false,
        "stream": true,
        "client_metadata": {
            "session_id": "0198f2b0-0000-7000-8000-000000000001",
            "thread_id": "0198f2b0-0000-7000-8000-000000000002",
            "turn_id": "0198f2b0-0000-7000-8000-000000000003",
            "root_turn_id": "0198f2b0-0000-7000-8000-000000000003",
            "x-codex-window-id": "0198f2b0-0000-7000-8000-000000000002:1",
            "x-codex-installation-id": "installation-1",
            "guardian_credits_requested": "true",
            "mcp_attribution": "{\"status\":\"none\"}",
            "x-codex-turn-metadata": r#"{"request_kind":"turn","session_id":"0198f2b0-0000-7000-8000-000000000001","thread_id":"0198f2b0-0000-7000-8000-000000000002","turn_id":"0198f2b0-0000-7000-8000-000000000003","root_turn_id":"0198f2b0-0000-7000-8000-000000000003","window_id":"0198f2b0-0000-7000-8000-000000000002:1","window_number":1}"#,
        },
    })
    .to_string();
    let delivered = zstd::stream::encode_all(plain.as_bytes(), 3).expect("zstd encode");

    let resp = codex_transportd::transport::responses_v3::execute_responses_v3_raw(
        &pool,
        &ctx,
        Bytes::from(delivered),
    )
    .await
    .expect("v3 outbound construction must execute");
    assert_eq!(resp.status, 200, "mock upstream answered 200");
    let mut stream = resp.body;
    while let Some(chunk) = stream.next().await {
        chunk.expect("mock stream chunk");
    }
}
