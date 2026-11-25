use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::common::with_tool_results;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::sse::spawn_response_stream;
use crate::telemetry::SseTelemetry;
use bytes::Bytes;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestBody;
use codex_client::RequestCompression;
use codex_client::RequestTelemetry;
use codex_client::TransportError;
use codex_protocol::models::ToolResultBody;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::instrument;

pub struct ResponsesClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

#[derive(Default)]
pub struct ResponsesOptions {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
    pub compression: Compression,
    pub tool_results: Vec<(usize, usize, Arc<ToolResultBody>)>,
    pub turn_state: Option<Arc<OnceLock<String>>>,
}

impl<T: HttpTransport> ResponsesClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    pub async fn stream_request(
        &self,
        request: ResponsesApiRequest,
        options: ResponsesOptions,
    ) -> Result<ResponseStream, ApiError> {
        self.stream_request_with_observer(request, options, |_| {})
            .await
    }

    /// Observes the encoded request before private connector results are attached.
    #[instrument(
        name = "responses.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "/responses"
        )
    )]
    pub async fn stream_request_with_observer(
        &self,
        request: ResponsesApiRequest,
        options: ResponsesOptions,
        on_encoded: impl FnOnce(&bytes::Bytes),
    ) -> Result<ResponseStream, ApiError> {
        let ResponsesOptions {
            session_id,
            thread_id,
            session_source,
            extra_headers,
            compression,
            tool_results,
            turn_state,
        } = options;
        let body = EncodedJsonBody::encode(&request)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;

        on_encoded(body.shared_bytes());
        let mut headers = extra_headers;
        if let Some(ref thread_id) = thread_id {
            insert_header(&mut headers, "x-client-request-id", thread_id);
        }
        headers.extend(build_session_headers(session_id, thread_id));
        if let Some(subagent) = subagent_header(&session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        if !tool_results.is_empty()
            && let Ok(json) = std::str::from_utf8(body.as_bytes())
            && let Some(json) = with_tool_results(json, &tool_results)
        {
            let stream = self
                .stream_encoded(
                    body.with_private_json(json),
                    headers.clone(),
                    compression,
                    turn_state.clone(),
                )
                .await;
            // A redirect that preserves the request body must start over without private results.
            if !matches!(
                stream,
                Err(ApiError::Transport(TransportError::Http {
                    status: StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT,
                    headers: Some(ref headers),
                    ..
                })) if headers.get(http::header::LOCATION).is_some_and(|location| !location.is_empty())
            ) {
                return stream;
            }
        }
        self.stream_encoded(body, headers, compression, turn_state)
            .await
    }

    #[instrument(
        name = "responses.stream",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "/responses",
            turn.has_state = turn_state.is_some()
        )
    )]
    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;
        self.stream_encoded(body, extra_headers, compression, turn_state)
            .await
    }

    /// Raw upstream Responses stream for the transport sidecar
    /// (TLS-CTD-001 FR-011, R6 preservation).
    ///
    /// Returns the upstream status, headers and byte stream untouched — no SSE
    /// parsing or re-serialization. Terminal non-2xx responses (e.g. 403, or
    /// 429/5xx after the official retry budget) are returned as `Ok` with
    /// their exact body and the real attempt count, so the sidecar can relay
    /// them byte-for-byte with `x-ctd-origin: upstream`.
    #[instrument(
        name = "responses.stream_raw",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses",
            turn.has_state = turn_state.is_some()
        )
    )]
    pub async fn stream_raw(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<codex_client::RawRetryOutcome<codex_client::StreamResponse>, ApiError> {
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };
        self.session
            .stream_raw_with(
                Method::POST,
                "/responses",
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.compression = request_compression;
                },
            )
            .await
    }

    /// v3（hookpoint=v3）raw 透传：body 是 Go 交付的**最终字节**，
    /// headers 是 Go 交付的**完整出站头集**，ctd 原样发出
    /// （docs/20 §3.5 / §4.3 M2）。
    ///
    /// ★ 与 [`Self::stream_raw`] 的关键差异：
    /// - body 走 [`RequestBody::Raw`] 且 compression 恒为
    ///   [`RequestCompression::None`]。该组合下 `prepare_body_for_send`
    ///   只把 headers 与 body 原样 clone——**既不写也不改**
    ///   `content-encoding` / `content-type`（见
    ///   `codex-http-client/src/request.rs` 的 `Raw` 分支）；
    /// - **不经 `configure` 闭包**：v3 下 ctd 不写 `accept`、不设
    ///   compression。`accept` / `content-type` 由 Go 交付，缺失由调用方
    ///   在发出前 fail closed。
    ///
    /// 由此得到 §3.3 R1/R3 的结构性保证：**ctd 不可能"压了没写头"或
    /// "写了头没压"**——`Raw` + 压缩会直接报错，而这里压缩恒为 `None`。
    /// 认证注入、retry、SSE 转发与 v1 共用同一 `EndpointSession` 闭包。
    #[instrument(
        name = "responses.stream_raw_passthrough",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses"
        )
    )]
    pub async fn stream_raw_passthrough(
        &self,
        body: Bytes,
        extra_headers: HeaderMap,
    ) -> Result<codex_client::RawRetryOutcome<codex_client::StreamResponse>, ApiError> {
        self.session
            .stream_raw_bytes_with(
                Method::POST,
                "/responses",
                extra_headers,
                Some(RequestBody::Raw(body)),
            )
            .await
    }

    /// v3 raw compact 透传（与 [`Self::stream_raw_passthrough`] 同构）：
    /// body 原样、headers 原样、timeout 显式传入。
    pub async fn execute_raw_passthrough(
        &self,
        body: Bytes,
        extra_headers: HeaderMap,
        request_timeout: Duration,
    ) -> Result<codex_client::RawRetryOutcome<codex_client::Response>, ApiError> {
        self.session
            .execute_raw_bytes_with(
                Method::POST,
                "/responses/compact",
                extra_headers,
                Some(RequestBody::Raw(body)),
                Some(request_timeout),
            )
            .await
    }

    /// Raw upstream compact execution for the transport sidecar
    /// (TLS-CTD-001 FR-012, R6 preservation).
    ///
    /// Upstream removed the dedicated `CompactClient` when it dropped the
    /// legacy remote compaction implementation; the `responses/compact` route is
    /// owned by the responses client here. Returns the upstream status, headers
    /// and body bytes untouched. Terminal non-2xx responses (e.g. 403, or
    /// 429/5xx after the official retry budget) are returned as `Ok` with the
    /// real attempt count. The caller (Go) decides window advance only on a
    /// confirmed complete 2xx success; timeout/EOF/5xx/parse failure never
    /// advance.
    ///
    /// ★ 出站编码与 [`Self::stream_raw`] **同一权威**：`compression` 决定
    /// body 字节与 `content-encoding`（两者由 `http-client` 的压缩路径一并
    /// 写入，结构上不可能「压了没写头」或「写了头没压」），`content-length`
    /// 由传输栈按压缩后字节数生成。`into_prepared` 在 `auth.apply_auth`
    /// 之前定稿 ⇒ `content-encoding` / `content-type` 落在 auth 之前
    /// （见 [`EndpointSession::execute_raw_encoded_with`]）。
    ///
    /// ★ `accept` 固定 `application/json`（不是 responses 的
    /// `text/event-stream`）：compact 上游是 **unary JSON** 协议，与 Go 侧
    /// R-Go 直连对同一 route 的强制值逐字一致
    /// （`sub2api backend/internal/service/openai_gateway_passthrough.go`：
    /// `if isOpenAIResponsesCompactPath(c) { req.Header.Set("accept",
    /// "application/json") }`，注释「compact 上游是 unary JSON 协议……
    /// 避免上游按 SSE 返回」）。两侧同 route 必须同值，否则 R-CTD 与 R-Go
    /// 出站头立刻对不上。
    pub async fn execute_raw_compact(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        request_timeout: Duration,
    ) -> Result<codex_client::RawRetryOutcome<codex_client::Response>, ApiError> {
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode compact request: {e}")))?;
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };
        self.session
            .execute_raw_encoded_with(
                Method::POST,
                "/responses/compact",
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("application/json"),
                    );
                    req.compression = request_compression;
                    req.timeout = Some(request_timeout);
                },
            )
            .await
    }

    async fn stream_encoded(
        &self,
        body: EncodedJsonBody,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                "/responses",
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.compression = request_compression;
                },
            )
            .await?;

        Ok(spawn_response_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
            turn_state,
        ))
    }
}
