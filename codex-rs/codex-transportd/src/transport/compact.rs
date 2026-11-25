//! Compact 原始适配（TLS-CTD-001 §12.2 / FR-012）。
//!
//! Rust 只执行官方 raw compact（`ResponsesClient::execute_raw_compact`）并原样返回
//! upstream status/headers/body；window advance 由 Go 在完整成功判定后
//! 执行（Rust 绝不推进 Go window）。

use std::time::Duration;

use bytes::Bytes;

use crate::contract::RawUpstreamResponse;
use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::transport::metadata;
use crate::transport::official_adapter;
use crate::transport::responses;

/// Compact 请求超时（官方 300s idle × 4）。
pub(crate) const COMPACT_REQUEST_TIMEOUT: Duration = Duration::from_secs(1200);

/// compact 出站头（v1）：`(元数据段, 尾段头)`，与 responses 同一权威。
///
/// ## 头序 —— **推断**（官方 compact 无样本）
///
/// 官方 `rust-v0.169.1` **不存在** `/responses/compact` 请求：
/// pristine `codex-api` 无 compact client，`official-1592/flows-*.jsonl` 里
/// 没有任何该 route 的请求行（`docs/23 §3.2` U2）。⇒ 本表按 **responses 的
/// 官方确定性构造序**（[`responses::outbound_headers`] /
/// `V1_METADATA_HEADER_ORDER`）对齐，这是**推断**，不是官方 compact 实测。
///
/// 支持该推断的旁证：官方 0.159.2 的压缩请求（remote compaction v2）走的是
/// **普通 `POST /responses`**（`core/src/compact_remote_v2.rs`
/// `run_remote_compaction_request_v2` → `ModelClientSession::stream` →
/// `stream_responses_api`）⇒ 官方 0.159.2 语境下的「压缩请求头序」= responses
/// 头序；且它与普通 turn 共用同一 `Provider` / `EndpointSession` /
/// `responses_request_compression`（`Compression::Zstd`）。
///
/// ## 与 responses 的头集差异（**不是照搬**）
///
/// 1. `x-codex-installation-id` —— compact **特有**。官方**最后一个**使用该
///    route 的 client（`rust-v0.153.4` 的 `CompactClient`）在 extra-headers
///    段**首位**插入它（`core/src/client.rs::compact_conversation_history`：
///    `extra_headers.insert(X_CODEX_INSTALLATION_ID_HEADER, …)` 先于
///    `build_responses_headers` / `add_originator_header` /
///    `build_responses_compatibility_headers` / `build_session_headers`），
///    官方测试亦断言该头存在（`core/tests/suite/compact_remote.rs`：
///    `compact_request.header("x-codex-installation-id")`）。故它排在
///    provider 的 `version` 之后、元数据段之前。
/// 2. `accept` —— 由 [`codex_api::ResponsesClient::execute_raw_compact`] 固定
///    为 `application/json`（unary JSON 协议，与 R-Go 直连同 route 同值），
///    **不取** responses 的 `text/event-stream`。
///
/// 值来源与优先级与改动前逐字等价（投影/会话快照 < 承载通道）：承载值覆盖
/// 同名的本地派生值（`HeaderMap::extend` 对同名键是替换，见
/// `http-1.4.0/src/header/map.rs` 的 `Extend<(Option<HeaderName>, T)>`
/// `Entry::Occupied` 分支）。
fn compact_outbound_headers(
    ctx: &RequestContext,
    installation_id: &str,
    projection: &codex_core::CodexResponsesMetadata,
) -> (http::HeaderMap, http::HeaderMap) {
    let (outbound, trailing) = responses::outbound_headers(ctx, projection);
    // installation 头先入 ⇒ 落在元数据段之前（官方 legacy compact 的插入序）。
    let mut headers = responses::installation_header(installation_id);
    headers.extend(outbound);
    (headers, trailing)
}

/// 执行官方 raw compact。
pub async fn execute_compact_raw(
    pool: &crate::transport::client_pool::ClientPool,
    installation_id: &str,
    ctx: &RequestContext,
    body: Bytes,
) -> Result<RawUpstreamResponse, LocalError> {
    // round-3 H2：重复 key fail closed。
    metadata::reject_duplicate_keys(std::str::from_utf8(&body).unwrap_or_default())?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        LocalError::new(
            "invalid_json",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "compact body is not valid JSON",
        )
    })?;
    metadata::validate_body_object(&value)?;
    if !metadata::big_int_roundtrip_safe(&value) {
        return Err(LocalError::new(
            "unsupported_integer_literal",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "body contains an integer literal outside u64 range",
        ));
    }

    let session = ctx.session.as_ref().ok_or_else(|| {
        LocalError::new(
            "missing_session",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "compact requires a session snapshot",
        )
    })?;
    // installation 身份 = 出站生效值（承载通道优先 ⇒ Go 按上游账号派生的值；
    // 否则回落 sidecar 进程级单值）。**一次解析**，projection 与 installation 头
    // 共用同一个值（头/体不得各自取值）。
    let installation_id = metadata::effective_installation_id(installation_id, ctx);
    let projection = metadata::build_projection(installation_id, session)?;

    // 出站头：与 responses **同一权威**（`responses::outbound_headers()` 的
    // 官方构造序），compact 只多一个 installation 头，见
    // [`compact_outbound_headers`]。`originator` / `user-agent` 与 responses
    // 同样交回官方位置（auth 之后、传输头之前）。
    let (headers, trailing_headers) = compact_outbound_headers(ctx, installation_id, &projection);

    // capture 旁路（capture-r1）：请求体校验通过后、发上游前入队；
    // 上游失败早退时句柄 Drop → 诚实 .resp.err 终态。
    let capture_handle = crate::capture::begin(ctx);
    if let Some(handle) = &capture_handle {
        handle.send_request(&body);
    }

    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::ResponsesCompact,
    )
    .await?;
    let auth = official_adapter::official_auth_with_trailing_headers(ctx, trailing_headers);
    let provider = official_adapter::chatgpt_provider();
    let api_client =
        codex_api::ResponsesClient::new(official_adapter::transport_for(client), provider, auth);

    // R6（R-TRANSPORT-001）：terminal non-2xx 原样保留（不映射为本地
    // 错误→502），attempt 次数来自官方 retry 循环的真实值。
    //
    // ⑤ `Compression::Zstd`：与 responses 同一策略（`docs/20 §3.4`
    // 「compact 同规则 = 同一承载通道 + 同一 Compression 语义」）。官方
    // 0.159.2 的出站 body 是 zstd 压缩体（`content-encoding: zstd`，magic
    // `28b52ffd`）；压缩与 `content-encoding` 由同一权威
    // （`http-client/src/request.rs::prepare_encoded_json`）写入，
    // `content-length` 由传输栈按**压缩后**字节数生成。
    let outcome = api_client
        .execute_raw_compact(
            value,
            headers,
            codex_api::Compression::Zstd,
            COMPACT_REQUEST_TIMEOUT,
        )
        .await
        .map_err(|err| responses::map_api_error(err, "compact"))?;
    let response = outcome.value;

    let status = response.status;
    let headers = crate::contract::filter_upstream_headers(&response.headers);
    let bytes = response.body;
    // F10（review）：compact unary 响应体同样有界（与 auxiliary 一致）。
    const MAX_UNARY_BODY: usize = 128 * 1024 * 1024;
    if bytes.len() > MAX_UNARY_BODY {
        return Err(crate::contract::error::LocalError::new(
            "upstream_body_too_large",
            crate::contract::error::ErrorClass::UpstreamProtocol,
            crate::contract::error::ErrorPhase::StreamBody,
            false,
            format!(
                "compact response body {} bytes exceeds unary limit {MAX_UNARY_BODY}",
                bytes.len()
            ),
        ));
    }

    // 响应旁路 tee（compact 同 responses，共 2 处 hook）。
    let body_stream: crate::contract::BodyStream = match capture_handle {
        Some(handle) => Box::pin(crate::capture::CaptureStream::new(
            futures::stream::once(Box::pin(async move { Ok::<_, std::io::Error>(bytes) })),
            handle,
        )),
        None => Box::pin(futures::stream::once(Box::pin(async move {
            Ok::<_, std::io::Error>(bytes)
        }))),
    };

    Ok(RawUpstreamResponse {
        status,
        headers,
        body: Box::pin(body_stream),
        attempt_count: u32::try_from(outcome.attempts).unwrap_or(u32::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::wire_harness::head_header_names;
    use crate::transport::wire_harness::header_value;
    use crate::transport::wire_harness::read_request;
    use std::collections::BTreeMap;
    use std::io::Write;

    /// 生产形态的承载集合（Go `codex_transport_sidecar.go` 的 11 名白名单中，
    /// v1 compact 会真的出现的那批）。
    fn carried_fixture() -> BTreeMap<String, String> {
        BTreeMap::from([
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
                r#"{"turn_id":"0198f2b0-0000-7000-8000-000000000003","request_kind":"turn"}"#
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
                "0198f2b0-0000-7000-8000-000000000004".to_string(),
            ),
            ("originator".to_string(), "codex-tui".to_string()),
            (
                "user-agent".to_string(),
                "codex-tui/0.159.0 (Ubuntu 22.4.0; x86_64) xterm-256color".to_string(),
            ),
        ])
    }

    fn ctx_fixture() -> RequestContext {
        RequestContext {
            request_id: "req-compact".to_string(),
            account_key: "acct".to_string(),
            credential_revision: "rev-1".to_string(),
            access_token: crate::redaction::SecretString::new("test-token"),
            chatgpt_account_id: Some(crate::redaction::SecretString::new("acct-id")),
            operation: Operation::ResponsesCompact,
            proxy_kind: crate::contract::ProxyKind::Direct,
            proxy_url: None,
            session: Some(crate::contract::headers::SessionSnapshot {
                session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
                thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
                window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
            }),
            carried_headers: carried_fixture(),
            ..Default::default()
        }
    }

    fn projection_fixture(ctx: &RequestContext) -> codex_core::CodexResponsesMetadata {
        metadata::build_projection(
            "installation-1",
            ctx.session.as_ref().expect("session snapshot"),
        )
        .expect("projection")
    }

    /// compact 元数据段的头名序列必须**逐字**等于 responses 的官方构造序，
    /// 且 `x-codex-installation-id` 排在其**前**（官方 legacy compact 的
    /// 插入序）。这是 `docs/23 §3.2` 登记的 v1 compact 头序缺陷的回归测试。
    #[test]
    fn compact_metadata_headers_follow_official_construction_order() {
        let ctx = ctx_fixture();
        let projection = projection_fixture(&ctx);
        let (headers, trailing) = compact_outbound_headers(&ctx, "installation-1", &projection);

        let names: Vec<&str> = headers.keys().map(http::HeaderName::as_str).collect();
        assert_eq!(
            names,
            vec![
                // compact 特有：官方 legacy compact 在 extra-headers 段首位插入。
                "x-codex-installation-id",
                // 以下 = `responses::V1_METADATA_HEADER_ORDER`（官方构造序），
                // 不是 BTreeMap 字母序。
                "x-codex-beta-features",
                "x-codex-window-id",
                "x-codex-turn-metadata",
                "x-openai-internal-codex-responses-lite",
                "x-codex-routing-hint",
                "x-client-request-id",
                "session-id",
                "thread-id",
            ]
        );
        assert_eq!(
            headers["x-codex-installation-id"], "installation-1",
            "installation 头必须取入参 installation_id"
        );
        // 承载值覆盖投影值：`x-codex-window-id` 来自承载（同值，但只允许一份）。
        assert_eq!(headers.get_all("x-codex-window-id").iter().count(), 1);
        assert_eq!(headers.get_all("session-id").iter().count(), 1);
        assert_eq!(headers.get_all("thread-id").iter().count(), 1);
        // `originator` / `user-agent` 交回官方位置（auth 之后），不在元数据段。
        assert!(!headers.contains_key("originator"));
        assert!(!headers.contains_key("user-agent"));
        let trailing_names: Vec<&str> = trailing.keys().map(http::HeaderName::as_str).collect();
        assert_eq!(trailing_names, vec!["originator", "user-agent"]);
        assert_eq!(trailing["originator"], "codex-tui");
    }

    /// ★ 安装标识的**生效值**闭环（v1 compact 腿）：承载的账号安装标识（Go
    /// `resolveAccountInstallationID`）覆盖 sidecar 进程级单值，且**只有一份**
    /// ——承载项被 `responses::outbound_headers` 消费掉，不会与注入的
    /// installation 头重复（compact 官方确实发该头）。
    #[test]
    fn compact_installation_header_follows_effective_carried_value() {
        let account_id = "0442ca22-065d-4351-bb85-aacd0b38faaf";
        let mut ctx = ctx_fixture();
        ctx.carried_headers.insert(
            crate::contract::headers::CARRIED_INSTALLATION_ID_KEY.to_string(),
            account_id.to_string(),
        );
        let effective = metadata::effective_installation_id("state-installation", &ctx);
        assert_eq!(effective, account_id);
        let projection =
            metadata::build_projection(effective, ctx.session.as_ref().expect("session snapshot"))
                .expect("projection");
        let (headers, _) = compact_outbound_headers(&ctx, effective, &projection);
        assert_eq!(headers["x-codex-installation-id"], account_id);
        assert_eq!(headers.get_all("x-codex-installation-id").iter().count(), 1);
        // body 侧同一份值（头/体不得各自取值）。
        assert_eq!(
            projection
                .client_metadata(false)
                .get("x-codex-installation-id")
                .map(String::as_str),
            Some(account_id)
        );

        // 不承载 ⇒ 回落进程级单值，且头集形态逐字相同（只换值）。
        let bare = ctx_fixture();
        let fallback = metadata::effective_installation_id("state-installation", &bare);
        assert_eq!(fallback, "state-installation");
        let projection =
            metadata::build_projection(fallback, bare.session.as_ref().expect("session snapshot"))
                .expect("projection");
        let (baseline_headers, _) = compact_outbound_headers(&bare, fallback, &projection);
        assert_eq!(
            baseline_headers["x-codex-installation-id"],
            "state-installation"
        );
        assert_eq!(
            baseline_headers
                .get_all("x-codex-installation-id")
                .iter()
                .count(),
            1
        );
        let names: Vec<&str> = baseline_headers
            .keys()
            .map(http::HeaderName::as_str)
            .collect();
        let carried_names: Vec<&str> = headers.keys().map(http::HeaderName::as_str).collect();
        assert_eq!(names, carried_names);
    }

    #[test]
    fn compact_timeout_is_bounded() {
        assert_eq!(COMPACT_REQUEST_TIMEOUT, Duration::from_secs(1200));
    }

    // -----------------------------------------------------------------------
    // 线上形态（真实 reqwest 传输 + 本地 mock 上游）
    // -----------------------------------------------------------------------

    /// ★ 验收项：compact 出站**真的 zstd 压缩**（`content-encoding: zstd` +
    /// zstd 帧 + `content-length` == 压缩后字节数），且头序为官方构造序
    /// （`accept` 在 `content-encoding` / `content-type` / `authorization`
    /// **之前**）。
    ///
    /// 走真实链路：ctd 头装配（[`compact_outbound_headers`]）→
    /// `codex_api::ResponsesClient::execute_raw_compact` →
    /// `EndpointSession::execute_raw_encoded_with`（`into_prepared` 定稿）→
    /// 官方 auth 注入 → 真实 `ReqwestTransport` → 本地 mock 记录原始字节。
    #[tokio::test]
    async fn compact_wire_is_zstd_encoded_in_official_header_order() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = std::sync::Arc::clone(&captured);
        let mock = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let (head, body) = read_request(&mut sock);
            *sink.lock().expect("lock") = Some((head, body));
            let payload = b"{\"output\":[]}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len()
            );
            sock.write_all(resp.as_bytes()).expect("write head");
            sock.write_all(payload).expect("write body");
            let _ = sock.flush();
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });

        let ctx = ctx_fixture();
        let projection = projection_fixture(&ctx);
        let (headers, trailing) = compact_outbound_headers(&ctx, "installation-1", &projection);

        // 与生产同形：provider 带 `version` 默认头（位置 0）。
        let mut provider_headers = http::HeaderMap::new();
        provider_headers.insert(
            "version",
            http::HeaderValue::from_str(official_adapter::codex_version()).expect("version"),
        );
        let provider = codex_api::Provider {
            name: "chatgpt".to_string(),
            base_url: format!("http://{addr}"),
            query_params: None,
            headers: provider_headers,
            retry: codex_api::RetryConfig {
                // 单次尝试：mock 只接受一个连接，重试会让断言变成噪声。
                max_attempts: 0,
                base_delay: Duration::from_millis(0),
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(300),
        };
        let auth = official_adapter::official_auth_with_trailing_headers(&ctx, trailing);
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("reqwest client");
        let api_client = codex_api::ResponsesClient::new(
            official_adapter::transport_for(client),
            provider,
            auth,
        );

        let plaintext = serde_json::json!({
            "model": "gpt-5.6-sol",
            "input": [{"role": "user", "content": "compact-marker"}],
            "stream": false,
        });
        let plaintext_bytes = serde_json::to_vec(&plaintext).expect("encode");

        let outcome = api_client
            .execute_raw_compact(
                plaintext.clone(),
                headers,
                codex_api::Compression::Zstd,
                Duration::from_secs(30),
            )
            .await
            .expect("compact request must reach the mock upstream");
        assert_eq!(outcome.value.status.as_u16(), 200);
        mock.join().expect("mock thread");
        let (head, body) = captured
            .lock()
            .expect("lock")
            .take()
            .expect("mock captured the request");

        // ① 真压缩：头声明 ⇔ 线上字节是 zstd 帧。
        assert_eq!(
            header_value(&head, "content-encoding"),
            Some("zstd"),
            "compact 出站必须声明 content-encoding: zstd\n{head}"
        );
        assert_eq!(
            &body[..4],
            &[0x28, 0xb5, 0x2f, 0xfd],
            "线上 body 必须是 zstd 帧（magic 28b52ffd）"
        );
        let decoded = zstd::stream::decode_all(std::io::Cursor::new(&body)).expect("zstd decode");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&decoded).expect("json"),
            plaintext,
            "解压后必须逐值等于请求体"
        );

        // ② content-length 与压缩后字节数同步（传输栈按最终字节生成）。
        let content_length: usize = header_value(&head, "content-length")
            .expect("content-length")
            .parse()
            .expect("content-length is a number");
        assert_eq!(
            content_length,
            body.len(),
            "content-length 必须等于线上字节数"
        );
        assert_ne!(
            content_length,
            plaintext_bytes.len(),
            "压缩后长度必须不同于明文长度（否则等于没压）"
        );

        // ③ 头序：compact 特有 installation 头紧随 version，其余为官方构造序。
        let names = head_header_names(&head);
        assert!(
            names.starts_with(&[
                "version".to_string(),
                "x-codex-installation-id".to_string(),
                "x-codex-beta-features".to_string(),
                "x-codex-window-id".to_string(),
                "x-codex-turn-metadata".to_string(),
                "x-openai-internal-codex-responses-lite".to_string(),
                "x-codex-routing-hint".to_string(),
                "x-client-request-id".to_string(),
                "session-id".to_string(),
                "thread-id".to_string(),
                "accept".to_string(),
                "content-encoding".to_string(),
                "content-type".to_string(),
                "authorization".to_string(),
                "chatgpt-account-id".to_string(),
                "originator".to_string(),
                "user-agent".to_string(),
            ]),
            "compact 出站头序必须为「version → installation → 官方元数据序 → accept \
             → content-encoding → content-type → auth → originator → user-agent」\n{names:?}"
        );
        // `accept` 是 unary JSON（与 R-Go 直连同 route 同值），不是 responses 的
        // `text/event-stream`。
        assert_eq!(header_value(&head, "accept"), Some("application/json"));
        assert_eq!(
            header_value(&head, "content-type"),
            Some("application/json")
        );
    }
}
