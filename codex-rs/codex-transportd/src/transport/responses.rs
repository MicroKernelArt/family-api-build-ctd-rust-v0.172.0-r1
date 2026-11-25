//! Responses HTTP/SSE 原始适配（TLS-CTD-001 §12.1 / FR-011）。
//!
//! 复用官方 `ResponsesClient::stream_raw`（fork patch），返回上游
//! status/headers/byte stream；SSE 不 parse 再重组，不全量缓冲。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;

use crate::contract::RawUpstreamResponse;
use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::transport::metadata;
use crate::transport::official_adapter;

/// Responses 专用 session headers（与官方 `build_session_headers` 一致）。
pub fn session_headers(ctx: &RequestContext) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(session) = &ctx.session {
        if let Ok(value) = HeaderValue::from_str(&session.session_id) {
            headers.insert("session-id", value);
        }
        if let Ok(value) = HeaderValue::from_str(&session.thread_id) {
            headers.insert("thread-id", value);
        }
    }
    headers
}

/// 官方 installation header（compact/HTTP 形态）。
pub fn installation_header(installation_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(installation_id) {
        headers.insert("x-codex-installation-id", value);
    }
    headers
}

/// 官方 compatibility headers（window/turn-metadata 等）。
pub fn compatibility_headers(projection: &codex_core::CodexResponsesMetadata) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in projection.compatibility_headers() {
        let Some(name) = name else { continue };
        headers.insert(name, value);
    }
    headers
}

/// v1 出站「ctd 自派生元数据头」的**官方确定性构造序**。
///
/// 官方 0.159.2 的 HTTP POST 头序列是**强不变量**（5 组采集 × 90 条 POST
/// 零变异，`docs/21 §0.3`）：
/// `version` → **本表 8 头** → `accept` → `content-encoding` → `content-type`
/// → `authorization` → `chatgpt-account-id` → `originator` → `user-agent`
/// → `host` → `content-length`。
///
/// 该序列直接来自官方的插入序：`Provider::build_request`（provider 头）→
/// `EndpointSession::make_request` 的 `headers.extend(extra_headers)` →
/// `configure` 闭包（`accept`）→ 压缩路径（`content-encoding` /
/// `content-type`）→ `auth.apply_auth`（凭据）→ reqwest client 默认头
/// （`originator` / `user-agent`）→ hyper 传输头（`host` / `content-length`）。
///
/// ★ 本表必须按官方构造序写死：改用 `BTreeMap` / `HashMap` 迭代序会得到
/// **字母序**，那是 `docs/22 §2` 实测的旧 v1 头序，与官方不符。
///
/// 不在本表的头由各自权威按同一顺序承载：`version` = provider，
/// `originator` / `user-agent` = [`V1_TRAILING_HEADER_ORDER`]，
/// `accept` / `content-encoding` / `content-type` = `configure` 与压缩路径，
/// `authorization` / `chatgpt-account-id` = auth，`host` / `content-length`
/// = 传输栈。
pub const V1_METADATA_HEADER_ORDER: &[&str] = &[
    "x-codex-beta-features",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-openai-internal-codex-responses-lite",
    "x-codex-routing-hint",
    "x-client-request-id",
    "session-id",
    "thread-id",
];

/// 官方交由 **reqwest client 默认头**承载的两个头，位置在 auth 之后、传输头
/// 之前（官方序列第 15/16 位）。v1 由承载通道拿到它们，必须复用同一注入点
/// （[`official_adapter::official_auth_with_trailing_headers`]）才能落在官方位置。
pub const V1_TRAILING_HEADER_ORDER: &[&str] = &["originator", "user-agent"];

/// v1 出站头投影：按官方构造序组装「ctd 自派生」段，并把官方由 client 默认头
/// 承载的 [`V1_TRAILING_HEADER_ORDER`] 单独交出。
///
/// 值来源与优先级与旧实现逐字等价（投影/会话快照 < 承载通道）：
/// `session-id` / `thread-id` 来自会话快照，`x-codex-window-id` /
/// `x-codex-turn-metadata` 来自官方投影，其余来自承载通道；承载值覆盖同名的
/// 本地派生值（`HeaderMap::insert` 的替换语义）。
///
/// 未列入两张表的头（如 `openai-beta`）按**确定性键序**追加在自派生段之后——
/// 官方 HTTP POST 不发这些头（`docs/20 §3.2`），此处只保证「不丢、不乱序」。
///
/// ★ 唯一的**消费型**白名单项是 `x-codex-installation-id`
/// （[`crate::contract::headers::CARRIED_INSTALLATION_ID_KEY`]）：它承载的是
/// projection 的 `installation_id` **来源**（见
/// [`metadata::effective_installation_id`]），不是出站头——官方 `/responses`
/// 不发该头（账号隔离由 body `client_metadata` 表达）。因此这里显式消费掉，
/// 绝不让它落进"未列名头按序追加"的分支。compact 侧的 installation 头由
/// [`crate::transport::compact::compact_outbound_headers`] 单独注入（官方 legacy
/// compact 确实发这个头）。
pub fn outbound_headers(
    ctx: &RequestContext,
    projection: &codex_core::CodexResponsesMetadata,
) -> (HeaderMap, HeaderMap) {
    let mut merged: BTreeMap<String, HeaderValue> = BTreeMap::new();
    for source in [
        session_headers(ctx),
        compatibility_headers(projection),
        ctx.carried_header_map(),
    ] {
        for (name, value) in source.iter() {
            merged.insert(name.as_str().to_string(), value.clone());
        }
    }
    merged.remove(crate::contract::headers::CARRIED_INSTALLATION_ID_KEY);

    let mut headers = HeaderMap::new();
    for name in V1_METADATA_HEADER_ORDER {
        if let Some(value) = merged.remove(*name) {
            headers.insert(http::header::HeaderName::from_static(name), value);
        }
    }

    // `originator` / `user-agent`：官方位置在 auth 之后 ⇒ 交给尾段注入点。
    // UA 形状在此归一（版本段取单一来源、按 originator 补官方尾后缀）。
    let originator = merged.remove("originator");
    let originator_str = originator
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let user_agent = official_adapter::canonical_user_agent(
        merged.remove("user-agent"),
        originator_str.as_deref(),
    );
    let mut trailing = HeaderMap::new();
    if let Some(value) = originator {
        trailing.insert(http::header::HeaderName::from_static("originator"), value);
    }
    if let Ok(value) = HeaderValue::from_str(&user_agent) {
        trailing.insert(http::header::USER_AGENT, value);
    }

    for (name, value) in merged {
        if let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) {
            headers.insert(name, value);
        }
    }
    (headers, trailing)
}

/// 执行 Responses HTTP/SSE 原始请求。
pub async fn execute_responses_raw(
    pool: &crate::transport::client_pool::ClientPool,
    installation_id: &str,
    ctx: &RequestContext,
    body: Bytes,
) -> Result<RawUpstreamResponse, LocalError> {
    // 1. 上游 I/O 前校验 JSON/body/marker（§16.2 顺序 6）。
    // round-3 H2：重复 key fail closed（serde_json 默认后者覆盖）。
    metadata::reject_duplicate_keys(std::str::from_utf8(&body).unwrap_or_default())?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        LocalError::new(
            "invalid_json",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "responses body is not valid JSON",
        )
    })?;
    metadata::validate_body_object(&value)?;
    let extra = metadata::extract_client_metadata(value.as_object().expect("object"))?;
    if !metadata::big_int_roundtrip_safe(&value) {
        return Err(LocalError::new(
            "unsupported_integer_literal",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "body contains an integer literal outside u64 range",
        ));
    }

    // 2. 官方投影（同一不可变 snapshot；reserved 键 fail closed）。
    let session = ctx.session.as_ref().ok_or_else(|| {
        LocalError::new(
            "missing_session",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "responses requires a session snapshot",
        )
    })?;
    let projection = metadata::build_projection(
        metadata::effective_installation_id(installation_id, ctx),
        session,
    )?;
    // F12（review）：直接拿所有权，消灭整树深拷贝（value 之后不再使用）。
    let serde_json::Value::Object(mut obj) = value else {
        return Err(LocalError::new(
            "invalid_json",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "responses body must be a JSON object",
        ));
    };
    // sidecar 投影对齐官方内置 OpenAI provider（include_internal_metadata = true）的出站形态。
    // try_for_transport 不设 mcp_attribution，本参数当前不改变投影内容，但必须与官方语义一致。
    metadata::inject_official_client_metadata(
        &mut obj,
        extra,
        projection.client_metadata(/*include_internal*/ true),
    );
    // canonical 位对齐：官方 HTTP 把 turn-metadata 同时放在 body client_metadata
    // 与 compatibility header；projection 由 snapshot 合成拿不到该值，用承载值补。
    metadata::inject_carried_turn_metadata(&mut obj, &ctx.carried_headers);
    // 官方 `client_metadata` 中承载通道**无权交付**（reserved）而官方必发的键：
    // `turn_id` / `root_turn_id`（由承载的 turn-metadata 派生）+
    // `guardian_credits_requested` / `mcp_attribution`（由 ctd 的会话模型派生）。
    metadata::inject_derived_client_metadata(&mut obj, ctx);

    // 3. extra headers：按官方**确定性构造序**组装（不是字母序）。
    //    `originator` / `user-agent` 由官方 client 默认头承载，位置在 auth
    //    之后 ⇒ 单独交出，见 `V1_TRAILING_HEADER_ORDER`。
    //    凭据头（authorization / chatgpt-account-id / cookie）在解析期已 fail
    //    closed，不在承载集合内；本地必需头（host/content-length/accept/
    //    content-type/content-encoding）与 authorization / chatgpt-account-id
    //    仍由官方 client、压缩路径与 BearerAuthProvider 自写。
    let (headers, trailing_headers) = outbound_headers(ctx, &projection);

    // I8：`body.prompt_cache_key` 随**出站** session-id（官方根会话两者同源，
    // 基准表 `invariants.I8` = PASS；v1 的原始值是客户端自己的会话标识，
    // 见 `metadata::rewrite_prompt_cache_key` 的文档）。改写值就取上一步定稿的
    // 出站头 `session-id` ⇒ 与 v1 写入 header / body
    // `client_metadata.session_id` 的是同一个值，不新增第二个算 session-id 的
    // 地方；只影响该键（`docs/24 §4.2 b`）。
    if let Some(outbound_session_id) = headers
        .get("session-id")
        .and_then(|value| value.to_str().ok())
    {
        metadata::rewrite_prompt_cache_key(&mut obj, outbound_session_id);
    }

    // capture 旁路（capture-r1）：请求体校验通过后、发上游前入队
    // （设计 §6.3）。白名单不匹配 = None，零成本原路径。句柄在上游失败
    // 早退时 Drop → best-effort Done(ok=false) → 诚实 .resp.err 终态。
    let capture_handle = crate::capture::begin(ctx);
    if let Some(handle) = &capture_handle {
        handle.send_request(&body);
    }

    // 4. 池化官方 client + 官方 auth。
    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::Responses,
    )
    .await?;
    let auth = official_adapter::official_auth_with_trailing_headers(ctx, trailing_headers);
    let provider = official_adapter::chatgpt_provider();
    let api_client =
        codex_api::ResponsesClient::new(official_adapter::transport_for(client), provider, auth);

    // 5. 官方 raw stream（复用 EndpointSession/auth/compression/retry）。
    // R6（R-TRANSPORT-001）：terminal non-2xx 原样保留（不再映射为本地
    // 错误→502），attempt 次数来自官方 retry 循环的真实值。
    //
    // ⑤ `Compression::Zstd`：官方 0.159.2 的出站 body 是 zstd 压缩体
    // （`content-encoding: zstd`，magic `28b52ffd`，`docs/21 §5.1`）。压缩与
    // 头的写入由同一权威（`http-client/src/request.rs::prepare_encoded_json`）
    // 完成 ⇒ ctd 结构上不可能「压了没写头」或「写了头没压」；`content-length`
    // 由传输栈按**压缩后**字节数生成。压缩后的字节存进 `EncodedJsonBody`
    // （`prepared = true`），retry attempt 复用同一份，不重复压缩。
    let outcome = api_client
        .stream_raw(obj.into(), headers, codex_api::Compression::Zstd, None)
        .await
        .map_err(|err| map_api_error(err, "responses"))?;
    let stream = outcome.value;

    let status = stream.status;
    let headers = crate::contract::filter_upstream_headers(&stream.headers);
    let body_stream = stream
        .bytes
        .map(|chunk| chunk.map_err(|err| std::io::Error::other(err.to_string())));

    // 响应流旁路 tee（不改流类型；chunk 原样放行，延迟 +0）。
    let body_stream: crate::contract::BodyStream = match capture_handle {
        Some(handle) => Box::pin(crate::capture::CaptureStream::new(body_stream, handle)),
        None => Box::pin(body_stream),
    };

    Ok(RawUpstreamResponse {
        status,
        headers,
        body: Box::pin(body_stream),
        attempt_count: u32::try_from(outcome.attempts).unwrap_or(u32::MAX),
    })
}

/// 将官方 ApiError 映射为本地 typed error（§13 所有权矩阵）。
pub fn map_api_error(err: codex_api::ApiError, operation: &str) -> LocalError {
    let _ = operation;
    match &err {
        codex_api::ApiError::Transport(transport_err) => match transport_err {
            codex_client::TransportError::Timeout => LocalError::new(
                "upstream_timeout",
                ErrorClass::Timeout,
                ErrorPhase::WaitHeaders,
                true,
                "upstream request timed out",
            ),
            codex_client::TransportError::Http { status, .. } => LocalError::new(
                "upstream_http_error",
                ErrorClass::Transport,
                ErrorPhase::WaitHeaders,
                false,
                format!("upstream returned HTTP {status}"),
            ),
            codex_client::TransportError::Network(..) => LocalError::new(
                "upstream_connect_failed",
                ErrorClass::Transport,
                ErrorPhase::Connect,
                true,
                "upstream network failure",
            ),
            codex_client::TransportError::Connection(..) => LocalError::new(
                "upstream_connect_failed",
                ErrorClass::Transport,
                ErrorPhase::Connect,
                true,
                "upstream connection failure",
            ),
            codex_client::TransportError::Build(..) => LocalError::new(
                "upstream_build_error",
                ErrorClass::Contract,
                ErrorPhase::WriteRequest,
                false,
                "upstream request build failure",
            ),
            codex_client::TransportError::RetryLimit => LocalError::new(
                "retry_limit_reached",
                ErrorClass::Transport,
                ErrorPhase::Connect,
                true,
                "official retry budget exhausted",
            ),
            // 既有变体：官方 transport 的有界 body 保护触发时，
            // 上游已经发出超过本地配置上限的响应体。与 compact/auxiliary 的
            // unary 上限同码同阶段：fail closed，不重试。
            codex_client::TransportError::ResponseTooLarge { max_bytes } => LocalError::new(
                "upstream_body_too_large",
                ErrorClass::UpstreamProtocol,
                ErrorPhase::StreamBody,
                false,
                format!("upstream response body exceeds {max_bytes} bytes"),
            ),
            // 0.159.0 新增变体：codex-http-client 的应用层网络策略拒绝。
            // NetworkPolicyDenied 是确定性拒绝（官方 RetryOn 判定不可重试、
            // api_bridge 映射为 Fatal），本地按 config 类 fail closed：
            // 不重试、绝不回退 direct。
            codex_client::TransportError::Policy(denied) => LocalError::new(
                "network_policy_denied",
                ErrorClass::Config,
                ErrorPhase::Connect,
                false,
                format!("destination denied by application network policy: {denied}"),
            ),
        },
        codex_api::ApiError::Stream(..) => LocalError::new(
            "upstream_stream_error",
            ErrorClass::UpstreamProtocol,
            ErrorPhase::StreamBody,
            false,
            "upstream stream error",
        ),
        _ => LocalError::new(
            "upstream_error",
            ErrorClass::Transport,
            ErrorPhase::WaitHeaders,
            false,
            "upstream error",
        ),
    }
}

#[allow(dead_code)]
fn _method_probe() -> Method {
    Method::POST
}

#[cfg(test)]
mod tests {
    use super::V1_METADATA_HEADER_ORDER;
    use super::V1_TRAILING_HEADER_ORDER;
    use super::outbound_headers;
    use super::session_headers;
    use crate::transport::metadata;
    use codex_core::CodexResponsesMetadata;
    use serde_json::Map;
    use serde_json::Value;
    use std::collections::BTreeMap;

    /// 生产形态的承载集合（Go `codex_transport_sidecar.go` 写入的 11 个白名单头
    /// 中，v1 responses 会真的出现的那批）+ 一个未列入官方 18 头的 `openai-beta`。
    fn carried_fixture() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("x-codex-beta-features".to_string(), "remote_compaction_v2".to_string()),
            (
                "x-codex-window-id".to_string(),
                "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
            ),
            (
                "x-codex-turn-metadata".to_string(),
                r#"{"turn_id":"0198f2b0-0000-7000-8000-000000000003","root_turn_id":"0198f2b0-0000-7000-8000-000000000003","request_kind":"turn"}"#
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
            (
                "openai-beta".to_string(),
                "responses_multi_agent=2026-01-01".to_string(),
            ),
        ])
    }

    fn projection() -> CodexResponsesMetadata {
        CodexResponsesMetadata::try_for_transport(
            "installation-1".to_string(),
            "0198f2b0-0000-7000-8000-000000000001".to_string(),
            "0198f2b0-0000-7000-8000-000000000002".to_string(),
            "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        )
        .expect("valid projection")
    }

    #[test]
    fn projection_roundtrips_official_metadata() {
        let metadata = CodexResponsesMetadata::try_for_transport(
            "installation-1".to_string(),
            "0198f2b0-0000-7000-8000-000000000001".to_string(),
            "0198f2b0-0000-7000-8000-000000000002".to_string(),
            "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        )
        .expect("valid projection");
        // 与生产投影一致：对齐官方内置 provider 的 include_internal_metadata = true。
        let client_metadata = metadata.client_metadata(/*include_internal*/ true);
        assert_eq!(
            client_metadata.get("session_id").unwrap(),
            "0198f2b0-0000-7000-8000-000000000001"
        );
        assert_eq!(
            client_metadata.get("x-codex-window-id").unwrap(),
            "0198f2b0-0000-7000-8000-000000000002:1"
        );
    }

    #[test]
    fn metadata_headers_follow_official_construction_order_not_alphabetical() {
        // ① 官方 0.159.2 的头名序列是强不变量（docs/21 §0.3：5 组采集 × 90 条
        // POST 零变异）。ctd 自派生段必须是官方构造序；字母序（BTreeMap 迭代序）
        // 正是 docs/22 §2 实测的旧 v1 头序。
        let ctx = crate::contract::RequestContext {
            session: Some(crate::contract::headers::SessionSnapshot {
                session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
                thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
                window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
            }),
            carried_headers: carried_fixture(),
            ..Default::default()
        };
        let (headers, trailing) = outbound_headers(&ctx, &projection());

        // 官方序列第 2–9 位，逐位相等。
        let observed: Vec<&str> = headers.keys().map(|name| name.as_str()).collect();
        assert_eq!(
            &observed[..V1_METADATA_HEADER_ORDER.len()],
            V1_METADATA_HEADER_ORDER,
            "ctd 自派生头必须按官方构造序排列"
        );
        // 反证：字母序不是官方序。
        let mut alphabetical = observed.clone();
        alphabetical.sort_unstable();
        assert_ne!(observed, alphabetical, "官方构造序不得退化为字母序");

        // `originator` / `user-agent` 必须离开自派生段（官方位置在 auth 之后）。
        assert!(headers.get("originator").is_none());
        assert!(headers.get("user-agent").is_none());
        let trailing_order: Vec<&str> = trailing.keys().map(|name| name.as_str()).collect();
        assert_eq!(trailing_order, V1_TRAILING_HEADER_ORDER.to_vec());

        // 未列入官方 18 头的承载头不丢，且不插进官方前缀。
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses_multi_agent=2026-01-01"
        );
        assert_eq!(observed.last().copied(), Some("openai-beta"));
    }

    /// ★ 消费型白名单项（`x-codex-installation-id`）的 v1 行为：
    /// 承载它**不得**改变 v1 `/responses` 的出站头集——它是 projection 的
    /// `installation_id` 来源，官方 `/responses` 不发这个头。
    ///
    /// 断言口径是"逐字不变"：同一 ctx 加上/不加该承载项，头名序列与尾段完全相同，
    /// 且两侧都**没有**该头。
    #[test]
    fn carried_installation_id_is_consumed_and_never_emitted_on_v1_responses() {
        let baseline = crate::contract::RequestContext {
            session: Some(crate::contract::headers::SessionSnapshot {
                session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
                thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
                window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
            }),
            carried_headers: carried_fixture(),
            ..Default::default()
        };
        let mut with_installation = baseline.clone();
        with_installation.carried_headers.insert(
            crate::contract::headers::CARRIED_INSTALLATION_ID_KEY.to_string(),
            "0442ca22-065d-4351-bb85-aacd0b38faaf".to_string(),
        );

        let (before, before_trailing) = outbound_headers(&baseline, &projection());
        let (after, after_trailing) = outbound_headers(&with_installation, &projection());

        let names = |map: &http::HeaderMap| -> Vec<String> {
            map.keys().map(|name| name.as_str().to_string()).collect()
        };
        assert_eq!(
            names(&before),
            names(&after),
            "承载安装标识不得改变 v1 出站头集"
        );
        assert_eq!(
            names(&before_trailing),
            names(&after_trailing),
            "承载安装标识不得改变尾段头集"
        );
        assert!(after.get("x-codex-installation-id").is_none());
        assert!(after_trailing.get("x-codex-installation-id").is_none());
    }

    #[test]
    fn carried_values_override_local_projection() {
        // 承载通道是 FR-004 的唯一 header 通道：同名键覆盖本地派生值。
        let ctx = crate::contract::RequestContext {
            session: Some(crate::contract::headers::SessionSnapshot {
                session_id: "0198f2b0-0000-7000-8000-000000000009".to_string(),
                thread_id: "0198f2b0-0000-7000-8000-00000000000a".to_string(),
                window_id: "0198f2b0-0000-7000-8000-00000000000a:1".to_string(),
            }),
            carried_headers: BTreeMap::from([(
                "x-codex-beta-features".to_string(),
                "carried_beta".to_string(),
            )]),
            ..Default::default()
        };
        let (headers, _) = outbound_headers(&ctx, &projection());
        assert_eq!(
            headers.get("x-codex-beta-features").unwrap(),
            "carried_beta"
        );
        // 会话快照仍是 session-id / thread-id 的来源（承载未提供时）。
        assert_eq!(
            headers.get("session-id").unwrap(),
            "0198f2b0-0000-7000-8000-000000000009"
        );
        assert_eq!(
            headers.get("thread-id").unwrap(),
            "0198f2b0-0000-7000-8000-00000000000a"
        );
    }

    #[test]
    fn upstream_headers_never_contain_internal_namespace() {
        // T3 负向（红线）：x-ctd-user-id 等内部 header 绝不进入上游投影。
        // session_headers 由空白 HeaderMap 构造，仅官方 session-id/thread-id。
        let ctx = crate::contract::RequestContext {
            session: Some(crate::contract::headers::SessionSnapshot {
                session_id: "0198f2b0-0000-7000-8000-000000000001".to_string(),
                thread_id: "0198f2b0-0000-7000-8000-000000000002".to_string(),
                window_id: "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
            }),
            user_id: Some("3".to_string()),
            ..Default::default()
        };
        let upstream = session_headers(&ctx);
        assert!(upstream.get("x-ctd-user-id").is_none());
        for (name, _) in upstream.iter() {
            assert!(
                !name.as_str().starts_with("x-ctd-"),
                "internal header {name} leaked to upstream projection"
            );
        }
        // 投影全量（含承载段与尾段）同样不得出现内部命名空间。
        let ctx = crate::contract::RequestContext {
            carried_headers: carried_fixture(),
            user_id: Some("3".to_string()),
            ..ctx
        };
        let (headers, trailing) = outbound_headers(&ctx, &projection());
        for name in headers.keys().chain(trailing.keys()) {
            assert!(
                !name.as_str().starts_with("x-ctd-"),
                "internal header {name} leaked to upstream projection"
            );
        }
    }

    /// I8 的合成层断言（`baseline/official-1592.json:invariants.I8` /
    /// `docs/24 §4.2 b`）：v1 body 的 `prompt_cache_key` 与**出站头**
    /// `session-id` 同源 ⇒ `body.prompt_cache_key == 出站 session-id`；且这一步
    /// 只改这一个键（其余 JSON 路径逐字不变）。
    ///
    /// 入站 body 复刻 round-1 装置用的真实样本
    /// （`tools/ctd-parity/fixtures/inbound-codex-0159.json` 的 body：客户端自带
    /// `prompt_cache_key` = 客户端自己的会话标识）。
    #[test]
    fn i8_prompt_cache_key_equals_outbound_session_id_and_touches_nothing_else() {
        let mut ctx = crate::contract::RequestContext {
            session: Some(crate::contract::headers::SessionSnapshot {
                session_id: "01a0f7e2-8d7e-73b1-a4f0-d903f408d712".to_string(),
                thread_id: "01a0f7e2-8d7e-73b1-a4f0-d903f408d712".to_string(),
                window_id: "01a0f7e2-8d7e-73b1-a4f0-d903f408d712:1".to_string(),
            }),
            ..Default::default()
        };
        ctx.carried_headers = BTreeMap::from([(
            "session-id".to_string(),
            "01a0f7e2-8d7e-73b1-a4f0-d903f408d712".to_string(),
        )]);
        let (headers, _) = outbound_headers(&ctx, &projection());
        let outbound_session_id = headers
            .get("session-id")
            .expect("v1 必写出站 session-id")
            .to_str()
            .expect("ASCII session-id");

        let before: Map<String, Value> = serde_json::from_str(
            r#"{"model":"gpt-5.6-sol","instructions":"ctd parity probe","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":true,"store":false,"prompt_cache_key":"01a0f307-43d2-7a92-90b5-c0d6ec15eed3"}"#,
        )
        .expect("fixture body");
        let mut after = before.clone();
        metadata::rewrite_prompt_cache_key(&mut after, outbound_session_id);

        // I8：body.prompt_cache_key == 出站 session-id（此处就是出站头的值）。
        assert_eq!(after["prompt_cache_key"], outbound_session_id);
        // 「只影响一项」的可重算断言：除该键外零差异。
        assert_eq!(
            changed_paths(&Value::Object(before), &Value::Object(after)),
            vec!["/prompt_cache_key".to_string()]
        );
    }

    /// 客户端**没带** `prompt_cache_key` ⇒ 组装后也不得有该键（不凭空造键）。
    #[test]
    fn i8_absent_client_cache_key_stays_absent() {
        let mut obj: Map<String, Value> =
            serde_json::from_str(r#"{"model":"gpt-5.6-sol","stream":true}"#).expect("body");
        metadata::rewrite_prompt_cache_key(&mut obj, "01a0f7e2-8d7e-73b1-a4f0-d903f408d712");
        assert!(obj.get("prompt_cache_key").is_none());
    }

    /// 两棵 JSON 树间「值不同」的路径集合（`/a/b`），用于断言「只改了一个键」。
    fn changed_paths(before: &Value, after: &Value) -> Vec<String> {
        fn walk(before: &Value, after: &Value, path: &str, out: &mut Vec<String>) {
            match (before, after) {
                (Value::Object(before), Value::Object(after)) => {
                    for (key, value) in before {
                        let child = format!("{path}/{key}");
                        match after.get(key) {
                            Some(other) => walk(value, other, &child, out),
                            None => out.push(child),
                        }
                    }
                    for key in after
                        .keys()
                        .filter(|key| !before.contains_key(key.as_str()))
                    {
                        out.push(format!("{path}/{key}"));
                    }
                }
                _ if before != after => out.push(path.to_string()),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(before, after, "", &mut out);
        out.sort();
        out
    }

    #[test]
    fn invalid_session_fails_before_io() {
        let err = CodexResponsesMetadata::try_for_transport(
            "i".to_string(),
            "not-a-uuid".to_string(),
            "0198f2b0-0000-7000-8000-000000000002".to_string(),
            "0198f2b0-0000-7000-8000-000000000002:1".to_string(),
        )
        .expect_err("invalid session");
        assert!(err.to_string().contains("UUIDv7"));
    }
}
