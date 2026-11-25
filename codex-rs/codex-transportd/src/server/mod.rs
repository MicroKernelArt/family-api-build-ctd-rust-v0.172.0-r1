//! UDS-only HTTP/1.1 server（TLS-CTD-001 §11.1 / FR-024）。
//!
//! 生产只监听 unix stream socket（0660），无任何 TCP listener。
//! 封闭 operation 端点、admission 顺序、原始流式回传。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bytes::Bytes;
use futures::TryStreamExt as _;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as HyperBuilder;
use tokio::net::UnixListener;
use tokio::sync::Semaphore;

use crate::build_info;
use crate::config::Config;
use crate::contract::Hookpoint;
use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::limits;
use crate::observability::Metrics;
use crate::transport::client_pool::ClientPool;

mod http;
mod websocket;

#[cfg(test)]
#[path = "operation_metrics_tests.rs"]
mod operation_metrics_tests;

/// Server 共享状态。
pub struct ServerState {
    pub config: Config,
    pub installation_id: String,
    pub metrics: Arc<Metrics>,
    pub pool: Arc<ClientPool>,
    /// 全局 in-flight 并发上限（FR-021）。
    pub inflight: Arc<Semaphore>,
    /// WS upstream 连接上限（F-008：admission 原子占位，非事后计数）。
    pub ws_slots: Arc<Semaphore>,
}

pub async fn serve(config: Config) -> Result<(), String> {
    let installation_id = crate::bootstrap::ensure_installation_id(&config.codex_home).await?;

    // F-017：socket 单实例安全所有权。
    // 1. 拒绝 symlink 与非 socket 文件（防误删普通文件/链接重定向）。
    // 2. 相邻 lockfile 独占锁（双实例互斥）。
    // 3. 已有活动 socket：报告 already running，绝不删除。
    // 4. 明确 stale（连接拒绝）才 unlink。
    // 5. bind 前验证父目录（PS-M4）：非 symlink、非 world-writable，
    //    且（配置期望时）owner/group 与部署 init 准备一致——Rust 进程
    //    cap_drop:ALL 不做运行时 chown，权限由 init/镜像准备。
    // 锁文件句柄必须存活到进程退出（serve 作用域），否则锁自动释放。
    let _socket_lock = acquire_socket_ownership(&config.socket_path)?;
    if let Some(parent) = config.socket_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create socket dir {}: {e}", parent.display()))?;
        verify_socket_parent_dir(
            parent,
            config.socket_dir_expected_uid,
            config.socket_dir_expected_gid,
        )?;
    }

    let listener = UnixListener::bind(&config.socket_path)
        .map_err(|e| format!("bind unix socket {}: {e}", config.socket_path.display()))?;
    // 0660：共享固定 GID，非共享组无法连接（FR-024）。
    set_socket_permissions(&config.socket_path)?;
    // R4-M2：bind 后复核最终路径——仍是 unix socket（拒绝 symlink/普通
    // 文件替换）且非 symlink。
    let bound_meta = std::fs::symlink_metadata(&config.socket_path)
        .map_err(|e| format!("post-bind stat {}: {e}", config.socket_path.display()))?;
    use std::os::unix::fs::FileTypeExt as _;
    if bound_meta.file_type().is_symlink() || !bound_meta.file_type().is_socket() {
        return Err(format!(
            "bound path {} is not a unix socket (type check failed)",
            config.socket_path.display()
        ));
    }

    tracing::info!(
        socket = %config.socket_path.display(),
        "codex-transportd listening (uds only)"
    );

    // F-013：server 与 client pool 共享同一 Metrics 实例（观测一致）。
    let metrics = Arc::new(Metrics::default());
    let state = Arc::new(ServerState {
        config,
        installation_id,
        metrics: Arc::clone(&metrics),
        pool: Arc::new(ClientPool::new(metrics)),
        inflight: Arc::new(Semaphore::new(limits::MAX_INFLIGHT)),
        ws_slots: Arc::new(Semaphore::new(limits::MAX_WS_CONNECTIONS)),
    });

    // capture-r1：CAPTURE_ROOT 缺省空 = 功能关闭（零开销）。
    // Metrics 与 server 共享同一实例；writer 线程独占全部 fs 系统调用。
    if let Some(capture_config) = crate::capture::CaptureConfig::from_env() {
        if let Err(err) = crate::capture::Capture::init(capture_config, Arc::clone(&state.metrics))
        {
            tracing::warn!(error = %err, "capture disabled: init failed");
        }
    }

    let app = router().with_state(state);
    let connection_slots = Arc::new(Semaphore::new(limits::MAX_UDS_CONNECTIONS));

    loop {
        let permit = connection_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| format!("UDS connection semaphore closed: {err}"))?;
        let (stream, _addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                drop(permit);
                tracing::error!(error = %err, "uds accept failed");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let _connection_permit = permit;
            let io = TokioIo::new(stream);
            let service = hyper_util::service::TowerToHyperService::new(app);
            // upgrade 支持（WS 依赖）：serve_connection_with_upgrades 在 101
            // 后把连接交接给 axum WebSocketUpgrade（§11.7）。
            let _ = HyperBuilder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await;
        });
    }
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| format!("chmod 0660 {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_socket_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn router() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/_internal/v1/health/live", get(http::health_live))
        .route("/_internal/v1/health/ready", get(http::health_ready))
        .route("/_internal/v1/info", get(http::info))
        .route("/_internal/v1/capture/status", get(http::capture_status))
        .route("/_internal/v1/responses", post(http::handle_http_operation))
        .route(
            "/_internal/v1/responses/compact",
            post(http::handle_http_operation),
        )
        .route("/_internal/v1/models", get(http::handle_http_operation))
        .route(
            "/_internal/v1/alpha/search",
            post(http::handle_http_operation),
        )
        .route("/_internal/v1/usage", get(http::handle_http_operation))
        .route(
            "/_internal/v1/quota/credits",
            get(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/quota/consume",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/auth/oauth_refresh",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/auth/whoami",
            get(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/files/download",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/files/attachment",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/files/content",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/agent/task_register",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/responses/ws",
            get(websocket::handle_ws_upgrade),
        )
        // F5（review）：三个 P1 operation 此前无路由注册 → 裸 axum 404，
        // 现注册到统一 handle_http_operation，走 typed fail-closed
        // （capability_not_released，x-ctd-origin: local）。
        .route(
            "/_internal/v1/files/upload",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/realtime/calls",
            post(http::handle_http_operation),
        )
        .route(
            "/_internal/v1/realtime/sideband",
            get(http::handle_http_operation),
        )
        // F-012（round-3）：DefaultBodyLimit 必须在所有 .route() 之后添加，
        // 否则只包裹空 router、后续路由仍受 axum 默认 2 MiB 限制。
        .layer(axum::extract::DefaultBodyLimit::max(
            limits::max_body_bytes(),
        ))
}

/// 根据 operation 执行对应原始请求，返回 X-CTD-Origin 标注的响应。
pub(crate) async fn execute_operation(
    state: &Arc<ServerState>,
    ctx: &RequestContext,
    body: Bytes,
) -> Result<Response, LocalError> {
    // admission 有界等待（FR-021）：队列满时 500ms 内拒绝，不无界排队。
    // M3：等待上限 = min(admission_timeout, caller 剩余 deadline)——已过期
    // 请求不等待 500ms，立即 deadline 失败。
    // R4-M1：budget source 显式归因——configured 先到 → admission_rejected；
    // caller deadline 先到 → deadline_exceeded。不再用"是否存在 deadline"
    // 猜测。
    let configured_wait = std::time::Duration::from_millis(
        ctx.admission_timeout_ms
            .unwrap_or(limits::ADMISSION_WAIT_MS),
    )
    .min(std::time::Duration::from_millis(limits::ADMISSION_WAIT_MS));
    enum BudgetSource {
        Configured,
        CallerDeadline,
    }
    let (admission_wait, budget_source) = match ctx.deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining <= configured_wait {
                (remaining, BudgetSource::CallerDeadline)
            } else {
                (configured_wait, BudgetSource::Configured)
            }
        }
        None => (configured_wait, BudgetSource::Configured),
    };
    let permit = tokio::time::timeout(admission_wait, state.inflight.clone().acquire_owned())
        .await
        .map_err(|_| match budget_source {
            BudgetSource::CallerDeadline => LocalError::new(
                "operation_deadline_exceeded",
                ErrorClass::Timeout,
                ErrorPhase::WaitHeaders,
                true,
                "operation exceeded caller deadline during admission wait",
            ),
            BudgetSource::Configured => crate::contract::error::codes::admission_rejected(),
        })?
        .map_err(|_| crate::contract::error::codes::admission_rejected())?;

    // caller deadline（X-CTD-Deadline-Unix-Ms）是绝对时间戳；直接转换为
    // tokio Instant，绝不被 admission 等待时长顺延（review F6）。
    let deadline = ctx
        .deadline
        .map(tokio::time::Instant::from_std)
        .unwrap_or_else(|| tokio::time::Instant::now() + default_deadline_for(ctx.operation));
    let future = execute_operation_inner(state, ctx, body);
    match tokio::time::timeout_at(deadline, future).await {
        Ok(Ok(response)) => {
            // F-007：permit/deadline 持有到 body EOF/error/drop，而非
            // 仅 headers——慢 SSE/下载不再绕过并发上限。
            let observation = crate::observability::RequestObservation {
                operation: ctx.operation,
                proxy_kind: ctx.proxy_kind,
                purpose: ctx.purpose,
                hookpoint: ctx.hookpoint,
            };
            Ok(guarded_response(
                response,
                permit,
                deadline,
                state.metrics.clone(),
                observation,
            ))
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err(LocalError::new(
            "operation_deadline_exceeded",
            crate::contract::error::ErrorClass::Timeout,
            crate::contract::error::ErrorPhase::WaitHeaders,
            true,
            "operation exceeded caller deadline",
        )),
    }
}

fn default_deadline_for(operation: Operation) -> std::time::Duration {
    match operation {
        Operation::ResponsesCompact => std::time::Duration::from_secs(1300),
        Operation::Responses
        | Operation::FileDownload
        | Operation::AttachmentDownload
        | Operation::FileContentFetch => std::time::Duration::from_secs(3600),
        _ => std::time::Duration::from_secs(300),
    }
}

/// F-007：把响应 body 包装为 GuardedBody（permit 持有到流结束）。
fn guarded_response(
    response: Response,
    permit: tokio::sync::OwnedSemaphorePermit,
    deadline: tokio::time::Instant,
    metrics: std::sync::Arc<crate::observability::Metrics>,
    observation: crate::observability::RequestObservation,
) -> Response {
    let (parts, body) = response.into_parts();
    let stream = body
        .into_data_stream()
        .map_err(|err| std::io::Error::other(format!("upstream body stream error: {err}")));
    let guarded = GuardedBody {
        inner: stream,
        _permit: permit,
        deadline_sleep: Box::pin(tokio::time::sleep_until(deadline)),
        metrics,
        observation,
        finished: false,
    };
    Response::from_parts(parts, axum::body::Body::from_stream(guarded))
}

async fn execute_operation_inner(
    state: &Arc<ServerState>,
    ctx: &RequestContext,
    body: Bytes,
) -> Result<Response, LocalError> {
    // 开关接线（步骤 5）：入口打一条低基数日志，标注本次请求实际生效的
    // hookpoint。
    tracing::info!(
        operation = ctx.operation.metric_label(),
        hookpoint = ctx.hookpoint.as_str(),
        "executing operation"
    );
    // P1 平面（T-012/014/015）：契约已定义，独立 release gate 通过前
    // 不执行（INV-015 capability 未宣告）。
    let capability_not_released = || {
        LocalError::new(
            "capability_not_released",
            crate::contract::error::ErrorClass::Config,
            crate::contract::error::ErrorPhase::Admission,
            false,
            "P1 operation 尚未通过独立 contract/wire/canary gate，拒绝执行",
        )
    };
    let raw = match ctx.operation {
        Operation::Responses => match ctx.hookpoint {
            // v1：ctd 自建请求（默认实现，与开关落地前逐字相同）。
            Hookpoint::V1 => {
                crate::transport::responses::execute_responses_raw(
                    &state.pool,
                    &state.installation_id,
                    ctx,
                    body,
                )
                .await?
            }
            // v3：原样透传 Go 交付的完整出站头集与最终 body 字节。
            Hookpoint::V3 => {
                crate::transport::responses_v3::execute_responses_v3_raw(&state.pool, ctx, body)
                    .await?
            }
        },
        Operation::ResponsesCompact => match ctx.hookpoint {
            Hookpoint::V1 => {
                crate::transport::compact::execute_compact_raw(
                    &state.pool,
                    &state.installation_id,
                    ctx,
                    body,
                )
                .await?
            }
            Hookpoint::V3 => {
                crate::transport::responses_v3::execute_compact_v3_raw(&state.pool, ctx, body)
                    .await?
            }
        },
        Operation::Models => {
            crate::transport::auxiliary::execute_models_raw(&state.pool, ctx).await?
        }
        Operation::AlphaSearch => {
            crate::transport::auxiliary::execute_alpha_search_raw(&state.pool, ctx, body).await?
        }
        Operation::Usage => {
            crate::transport::auxiliary::execute_wham_raw(&state.pool, ctx, Operation::Usage, None)
                .await?
        }
        Operation::QuotaCredits => {
            crate::transport::auxiliary::execute_wham_raw(
                &state.pool,
                ctx,
                Operation::QuotaCredits,
                None,
            )
            .await?
        }
        Operation::QuotaConsume => {
            crate::transport::auxiliary::execute_wham_raw(
                &state.pool,
                ctx,
                Operation::QuotaConsume,
                Some(body),
            )
            .await?
        }
        Operation::OAuthWhoami => {
            crate::transport::auth::execute_whoami_raw(&state.pool, ctx).await?
        }
        Operation::OAuthRefresh => {
            crate::transport::auth::execute_refresh_raw(&state.pool, ctx).await?
        }
        Operation::FileDownload => {
            crate::transport::files::execute_file_download(&state.pool, ctx).await?
        }
        Operation::AttachmentDownload => {
            crate::transport::files::execute_attachment_download(&state.pool, ctx).await?
        }
        Operation::AgentTaskRegister => {
            crate::transport::agent::execute_agent_task_register_raw(&state.pool, ctx).await?
        }
        Operation::FileContentFetch => {
            let download_url = String::from_utf8(body.to_vec()).map_err(|_| {
                LocalError::new(
                    "invalid_download_url_body",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    "file_content_fetch body must be a UTF-8 download URL",
                )
            })?;
            crate::transport::files::execute_file_content_fetch(&state.pool, ctx, &download_url)
                .await?
        }
        Operation::ResponsesWebSocketV2 => {
            return Err(LocalError::new(
                "wrong_transport",
                crate::contract::error::ErrorClass::Contract,
                crate::contract::error::ErrorPhase::Admission,
                false,
                "responses WS must use the upgrade endpoint",
            ));
        }
        Operation::FileUpload | Operation::RealtimeCall | Operation::RealtimeSidebandWS => {
            return Err(capability_not_released());
        }
    };

    // R-OBS-001：真实 retry 计数按 operation/终态 outcome 记账（单一
    // 记账点；attempts<=1 零增量，无双计）。
    crate::observability::observe_retries(
        &state.metrics,
        ctx.operation,
        raw.attempt_count,
        raw.status.is_success(),
    );

    Ok(http::upstream_response(raw, ctx))
}

/// F-007：admission permit + deadline 覆盖响应 body 完整生命周期。
/// permit 在 Drop 时自动释放（EOF/error/客户端断连即释放）。
/// P2-5：deadline 用 `tokio::time::Sleep` 与 inner stream 竞速——即使
/// upstream stall（inner 不 poll 就绪），sleep 到期时 poll_next 也产出
/// typed 流错误，不依赖惰性时钟检查。
/// H4：Success/inflight 指标在 body EOF/error/drop 时结算（headers 后
/// 流式错误不再统计为成功）。
struct GuardedBody<S> {
    inner: S,
    _permit: tokio::sync::OwnedSemaphorePermit,
    deadline_sleep: std::pin::Pin<Box<tokio::time::Sleep>>,
    metrics: std::sync::Arc<crate::observability::Metrics>,
    observation: crate::observability::RequestObservation,
    finished: bool,
}

impl<S> GuardedBody<S> {
    fn finish_success(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        crate::observability::observe_request_end(
            &self.metrics,
            self.observation,
            crate::observability::Outcome::Success,
        );
    }

    fn finish_stream_error(&mut self, class: &str, phase: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        crate::observability::observe_error(&self.metrics, self.observation, class, phase);
        crate::observability::observe_request_end(
            &self.metrics,
            self.observation,
            crate::observability::Outcome::StreamError,
        );
    }
}

impl<S> Drop for GuardedBody<S> {
    fn drop(&mut self) {
        // 客户端断连/未完成：按 error 结算（inflight 必须归零）。
        self.finish_stream_error("drop", "body");
    }
}

impl<S> futures::Stream for GuardedBody<S>
where
    S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.finished {
            // M6：fused——错误/结束后不再 poll inner，直接返回 None。
            return std::task::Poll::Ready(None);
        }
        if self.deadline_sleep.as_mut().poll(cx).is_ready() {
            self.finish_stream_error("timeout", "body");
            return std::task::Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "response stream deadline exceeded",
            ))));
        }
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => std::task::Poll::Ready(Some(Ok(bytes))),
            std::task::Poll::Ready(Some(Err(err))) => {
                self.finish_stream_error("transport", "body");
                std::task::Poll::Ready(Some(Err(err)))
            }
            std::task::Poll::Ready(None) => {
                self.finish_success();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// 防止未来意外引入 TCP listener：编译期确认 crate 不依赖 tokio TcpListener
/// 类型（编译错误即失败）。
#[allow(dead_code)]
fn _no_tcp_listener_probe() -> Option<SocketAddr> {
    None
}

/// 本地错误渲染为 problem+json（X-CTD-Origin: local）。
pub(crate) fn local_error_response(err: &LocalError, request_id: &str) -> Response {
    let status = err.status();
    let body = err.problem_body(request_id);
    let json = serde_json::to_vec(&body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/problem+json")
        .header("x-ctd-origin", "local")
        .body(axum::body::Body::from(json))
        .expect("valid response")
}

/// 版本/能力信息（§11.2）。
pub(crate) fn info_json(state: &ServerState) -> serde_json::Value {
    serde_json::json!({
        "protocol_version": 1,
        "service": "codex-transportd",
        "release_id": build_info::RELEASE_ID,
        "codex_tag": build_info::UPSTREAM_TAG,
        "codex_version": env!("CARGO_PKG_VERSION"),
        "codex_commit": build_info::UPSTREAM_COMMIT,
        "cargo_lock_sha256": build_info::UPSTREAM_LOCK_SHA256,
        "fork_commit": build_info::FORK_COMMIT_FROM_ENV,
        "fork_cargo_lock_sha256": build_info::FORK_LOCK_FROM_ENV,
        "rust_toolchain": build_info::TOOLCHAIN,
        "target": build_info::TARGET,
        "image_digest": state.config.image_digest_override.clone()
            .unwrap_or_default(),
        // R7-F5：base_image_ref 与 image_digest 拆分——编译期 base image
        // 引用不得冒充实际 sidecar 镜像 digest；无 override 时 image_digest
        // 为空（Go 侧 pin 缺失即 fail-closed），base_image_ref 仅供审计。
        "base_image_ref": build_info::BASE_IMAGE_REF,
        "build_proof_digest": state.config.build_proof_digest_override.clone()
            .unwrap_or_else(|| build_info::BUILD_PROOF_FROM_ENV.to_string()),
        "capabilities": build_info::CAPABILITIES,
        // R8-F4：test-upstream 构建机器可证——Go 侧生产必拒
        //（AllowTestUpstreamFeature 缺省 false）。
        "test_upstream_enabled": cfg!(feature = "test-upstream"),
        "attestation": false,
    })
}

/// 外部 header 元数据（无 secret）。
pub(crate) fn extract_request_meta(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let request_id = headers
        .get("x-ctd-request-id")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let account_hash = headers
        .get("x-ctd-account-key")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    (request_id, account_hash)
}

/// F-017：安全获取 socket 所有权（symlink/类型/双实例/stale 判定）。
fn acquire_socket_ownership(socket_path: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::FileTypeExt;

    // 1. 存在性检查：必须是 unix socket（拒绝 symlink/普通文件/目录）。
    if let Ok(meta) = std::fs::symlink_metadata(socket_path) {
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "socket path {} is a symlink; refusing to touch it",
                socket_path.display()
            ));
        }
        if !file_type.is_socket() {
            return Err(format!(
                "socket path {} exists and is not a unix socket; refusing to remove it",
                socket_path.display()
            ));
        }
    }

    // 2. 相邻 lockfile 独占锁（双实例互斥；进程退出自动释放）。
    let lock_path = socket_path.with_extension("sock.lock");
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create socket dir {}: {e}", parent.display()))?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("cannot open socket lock {}: {e}", lock_path.display()))?;
    std::fs::File::try_lock(&lock_file).map_err(|e| {
        format!(
            "another codex-transportd instance owns {} (lock {}): {e}",
            socket_path.display(),
            lock_path.display()
        )
    })?;

    // 3. 已有 socket：探测是否活动（连接成功 = 另一个实例在服务，拒绝启动）。
    if socket_path.exists() {
        use std::os::unix::net::UnixStream;
        match UnixStream::connect(socket_path) {
            Ok(_) => {
                return Err(format!(
                    "socket {} is already served by another instance; refusing to start",
                    socket_path.display()
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                // M5：仅连接拒绝（ECONNREFUSED）视为 stale 可 unlink；
                // 权限/其他错误不得删除。
                std::fs::remove_file(socket_path).map_err(|e| {
                    format!("cannot remove stale socket {}: {e}", socket_path.display())
                })?;
            }
            Err(err) => {
                return Err(format!(
                    "socket {} 探测失败（非 stale）：{err};拒绝 unlink",
                    socket_path.display()
                ));
            }
        }
    }
    Ok(lock_file)
}

#[cfg(test)]
mod guarded_body_tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    #[test]
    fn default_deadline_exceeds_adapter_timeouts() {
        assert!(
            default_deadline_for(Operation::ResponsesCompact)
                > crate::transport::compact::COMPACT_REQUEST_TIMEOUT
        );
        assert_eq!(
            default_deadline_for(Operation::Responses),
            Duration::from_secs(3600)
        );
    }

    #[tokio::test]
    async fn permit_held_until_stream_end() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().expect("one slot");
        let inner = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(bytes::Bytes::from("a")),
            Ok::<_, std::io::Error>(bytes::Bytes::from("b")),
        ]);
        let mut body = GuardedBody {
            inner,
            _permit: permit,
            deadline_sleep: Box::pin(tokio::time::sleep_until(
                tokio::time::Instant::now() + Duration::from_secs(60),
            )),
            metrics: std::sync::Arc::new(crate::observability::Metrics::default()),
            observation: crate::observability::RequestObservation {
                operation: Operation::Responses,
                proxy_kind: crate::contract::headers::ProxyKind::Direct,
                purpose: crate::contract::headers::RequestPurpose::Request,
                hookpoint: crate::contract::headers::Hookpoint::V1,
            },
            finished: false,
        };
        assert_eq!(semaphore.available_permits(), 0, "permit 持有中");

        let mut count = 0;
        while let Some(item) = std::pin::Pin::new(&mut body).next().await {
            assert!(item.is_ok());
            count += 1;
        }
        assert_eq!(count, 2);
        drop(body);
        assert_eq!(semaphore.available_permits(), 1, "EOF 后 permit 释放");
    }

    #[tokio::test]
    async fn deadline_exceeded_yields_stream_error() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().expect("one slot");
        let inner = futures::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from("a"))]);
        let mut body = GuardedBody {
            inner,
            _permit: permit,
            deadline_sleep: Box::pin(tokio::time::sleep_until(
                tokio::time::Instant::now() - Duration::from_secs(1), // 已过期
            )),
            metrics: std::sync::Arc::new(crate::observability::Metrics::default()),
            observation: crate::observability::RequestObservation {
                operation: Operation::Responses,
                proxy_kind: crate::contract::headers::ProxyKind::Direct,
                purpose: crate::contract::headers::RequestPurpose::Request,
                hookpoint: crate::contract::headers::Hookpoint::V1,
            },
            finished: false,
        };
        let item = std::pin::Pin::new(&mut body).next().await.expect("item");
        assert!(item.is_err(), "deadline 过期必须产出流错误");
        drop(body);
        assert_eq!(semaphore.available_permits(), 1, "错误后 permit 释放");
    }
}

/// PS-M4：socket 父目录启动验证。
/// - 拒绝 symlink（防止目录被链接重定向到攻击者控制路径）；
/// - 拒绝 world-writable（防止其他 UID 替换 socket/文件）；
/// - 配置期望 uid/gid 时与目录 owner 精确比较（部署模型：独立 UID +
///   共享 GID，目录由 init 准备，Rust 不做运行时 chown）。
///
/// 验证失败立即退出，不进入监听。
pub(crate) fn verify_socket_parent_dir(
    parent: &std::path::Path,
    expected_uid: Option<u32>,
    expected_gid: Option<u32>,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    let meta = std::fs::symlink_metadata(parent)
        .map_err(|e| format!("cannot stat socket dir {}: {e}", parent.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "socket dir {} must not be a symlink",
            parent.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "socket parent {} is not a directory",
            parent.display()
        ));
    }
    let mode = meta.mode();
    if mode & 0o002 != 0 {
        return Err(format!(
            "socket dir {} is world-writable (mode {:o}), refusing to serve",
            parent.display(),
            mode & 0o777
        ));
    }
    if let Some(uid) = expected_uid
        && meta.uid() != uid
    {
        return Err(format!(
            "socket dir {} owner uid {} != expected {}",
            parent.display(),
            meta.uid(),
            uid
        ));
    }
    if let Some(gid) = expected_gid
        && meta.gid() != gid
    {
        return Err(format!(
            "socket dir {} group gid {} != expected {}",
            parent.display(),
            meta.gid(),
            gid
        ));
    }
    Ok(())
}

#[cfg(test)]
mod socket_dir_tests {
    use super::verify_socket_parent_dir;

    fn tmpdir() -> std::path::PathBuf {
        // 原子计数器保证并行测试下路径唯一（同 pid 同微秒时 nanos 会撞号，
        // 导致两个测试共享同一目录、权限断言互相污染）。
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "ctd-socket-dir-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn accepts_secure_dir() {
        let d = tmpdir();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o770)).unwrap();
        verify_socket_parent_dir(&d, None, None).expect("secure dir accepted");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rejects_world_writable() {
        let d = tmpdir();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = verify_socket_parent_dir(&d, None, None).expect_err("world-writable rejected");
        assert!(err.contains("world-writable"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rejects_symlink() {
        let real = tmpdir();
        let link = tmpdir().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = verify_socket_parent_dir(&link, None, None).expect_err("symlink rejected");
        assert!(err.contains("symlink"), "{err}");
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_dir_all(link.parent().unwrap()).ok();
    }

    #[test]
    fn rejects_uid_mismatch() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let d = tmpdir();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o770)).unwrap();
        let wrong = std::fs::metadata(&d).unwrap().uid().wrapping_add(1);
        let err = verify_socket_parent_dir(&d, Some(wrong), None).expect_err("uid mismatch");
        assert!(err.contains("owner uid"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }
}
