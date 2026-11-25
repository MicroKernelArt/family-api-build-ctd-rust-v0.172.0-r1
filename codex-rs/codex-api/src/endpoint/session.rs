use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::run_with_request_telemetry;
use crate::telemetry::run_with_request_telemetry_preserving_status;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RawRetryOutcome;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use http::HeaderMap;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

pub(crate) struct EndpointSession<T: HttpTransport> {
    transport: T,
    provider: Provider,
    auth: SharedAuthProvider,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
}

impl<T: HttpTransport> EndpointSession<T> {
    pub(crate) fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
        }
    }

    pub(crate) fn with_request_telemetry(
        mut self,
        request: Option<Arc<dyn RequestTelemetry>>,
    ) -> Self {
        self.request_telemetry = request;
        self
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    fn make_request(
        &self,
        method: &Method,
        path: &str,
        extra_headers: &HeaderMap,
        body: Option<&RequestBody>,
    ) -> Request {
        let mut req = self.provider.build_request(method.clone(), path);
        req.headers.extend(extra_headers.clone());
        if let Some(body) = body {
            req.body = Some(body.clone());
        }
        req
    }

    pub(crate) async fn execute(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        self.execute_with(method, path, extra_headers, body, |_| {})
            .await
    }

    #[instrument(
        name = "endpoint_session.execute_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<Response, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::Json);
        let make_request = || {
            let mut req = self.make_request(&method, path, &extra_headers, body.as_ref());
            configure(&mut req);
            req
        };

        let response = run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute(req).await
                }
            },
        )
        .await?;

        Ok(response)
    }

    #[instrument(
        name = "endpoint_session.stream_encoded_json_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn stream_encoded_json_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
    ) -> Result<StreamResponse, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        let stream = run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.stream(req).await
                }
            },
        )
        .await?;

        Ok(stream)
    }

    /// raw unary 形态（transportd R6）：任何 status 的响应原样返回
    /// （terminal non-2xx 不映射为 `ApiError`），并携带官方 retry 后的
    /// 真实 attempt 次数。重试语义与 [`Self::execute_with`] 一致。
    #[instrument(
        name = "endpoint_session.execute_raw_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_raw_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<RawRetryOutcome<Response>, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::Json);
        let make_request = || {
            let mut req = self.make_request(&method, path, &extra_headers, body.as_ref());
            configure(&mut req);
            req
        };

        run_with_request_telemetry_preserving_status(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute_preserving_status(req).await
                }
            },
        )
        .await
        .map_err(ApiError::from)
    }

    /// raw unary + 官方压缩形态（transportd FR-012 compact）：与
    /// [`Self::stream_raw_with`] **逐字同构**（`make_request` → `configure`
    /// → `into_prepared` → `auth.apply_auth` → 发出），只把「流式」换成
    /// 「unary」（`execute_preserving_status`）；与
    /// [`Self::execute_raw_with`] 只差 `EncodedJsonBody` + `into_prepared`。
    ///
    /// ★ 为什么必须 `into_prepared`（而不是 `RequestBody::Json` 直发）：
    /// 官方 `content-encoding` / `content-type` 由压缩路径与
    /// `prepare_encoded_json` 写入（`http-client/src/request.rs`），
    /// `into_prepared` 把它们**定稿在 `auth.apply_auth` 之前** ⇒ 出站序为
    /// `accept` → `content-encoding` → `content-type` → `authorization` →
    /// `chatgpt-account-id` → `originator` → `user-agent`，与官方
    /// `/responses` 的构造序一致。`execute_raw_with` 不做 `into_prepared`，
    /// `content-type` 会被推迟到传输栈（落在 auth 之后），序不同。
    ///
    /// `content-length` 不在本层：由传输栈按 `into_prepared` 定稿的
    /// **最终字节数**（压缩后）生成，头与体同一权威，结构上不可能不一致。
    #[instrument(
        name = "endpoint_session.execute_raw_encoded_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_raw_encoded_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
    ) -> Result<RawRetryOutcome<Response>, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        run_with_request_telemetry_preserving_status(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute_preserving_status(req).await
                }
            },
        )
        .await
        .map_err(ApiError::from)
    }

    /// raw 流式形态（transportd R6）：terminal non-2xx 以 `Ok` 返回且
    /// body 可逐块读取；attempt 次数真实。重试语义与
    /// [`Self::stream_encoded_json_with`] 一致。
    #[instrument(
        name = "endpoint_session.stream_raw_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn stream_raw_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
    ) -> Result<RawRetryOutcome<StreamResponse>, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        run_with_request_telemetry_preserving_status(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.stream_preserving_status(req).await
                }
            },
        )
        .await
        .map_err(ApiError::from)
    }

    /// v3（hookpoint=v3）raw 流式形态：body 是 Go 交付的**最终字节**，
    /// ctd 原样透传（docs/20 §3.5 / §4.3 M1）。
    ///
    /// 与 [`Self::stream_raw_with`] 的差异**只在输入**，落在
    /// `make_request → into_prepared` 这一段（`stream_raw_with` 的 224–226）：
    /// - `extra_headers` 已是 Go 交付的**完整出站头集**，且 **不调用
    ///   `configure` 闭包**——v3 下 ctd 不做任何 header/compression 决策；
    /// - `body` 是 [`RequestBody::Raw`]，不走 `EncodedJson` 的 JSON 编码与
    ///   压缩路径 ⇒ 字节逐字一致、键序与空白全部保留。
    ///
    /// 之后的一切与 v1 **逐字相同**：`into_prepared` 定稿 body，
    /// `request.clone()` 供每次 attempt（`Bytes` 引用计数克隆，零拷贝），
    /// `auth.apply_auth` 在发出前注入凭据，retry/超时/SSE 转发由官方
    /// telemetry + retry 循环承担。★ 认证注入闭包**不做任何改动**。
    #[instrument(
        name = "endpoint_session.stream_raw_bytes_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn stream_raw_bytes_with(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<RequestBody>,
    ) -> Result<RawRetryOutcome<StreamResponse>, ApiError> {
        let request = self.make_request(&method, path, &extra_headers, body.as_ref());
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        run_with_request_telemetry_preserving_status(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.stream_preserving_status(req).await
                }
            },
        )
        .await
        .map_err(ApiError::from)
    }

    /// v3（hookpoint=v3）raw unary 形态（compact 同构）：与
    /// [`Self::execute_raw_with`] 同形，但 body 走 [`RequestBody::Raw`] 且
    /// 不调用 `configure`。`timeout` 显式传入（v3 没有 configure 闭包可写
    /// `req.timeout`），其余（凭据注入 / retry / 状态保留）逐字同 v1。
    #[instrument(
        name = "endpoint_session.execute_raw_bytes_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_raw_bytes_with(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<RequestBody>,
        timeout: Option<std::time::Duration>,
    ) -> Result<RawRetryOutcome<Response>, ApiError> {
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        request.timeout = timeout;
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        run_with_request_telemetry_preserving_status(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute_preserving_status(req).await
                }
            },
        )
        .await
        .map_err(ApiError::from)
    }
}
