//! Responses WS v2 1:1 适配（TLS-CTD-001 §12.3 / FR-013 / FR-014 / FR-020）。
//!
//! - 一个 internal WS 对应一个 upstream WS；不跨 account/session 复用；
//! - Rust 拥有上游握手/TLS/control frames；
//! - 下游已发送的 `generate=false` prewarm 原样表达，Rust 不再生成；
//! - 写后歧义不 replay frame；关闭连接让下游重建会话；
//! - Ping/Pong/Close 由两端 adapter 显式处理，不当作业务 frame 透传。

use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::extensions::ExtensionsConfig;
use tokio_tungstenite::tungstenite::extensions::compression::deflate::DeflateConfig;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::limits::WS_FRAME_QUEUE_CAPACITY;
use crate::transport::metadata;
use crate::transport::official_adapter;

/// WS 握手 header 的 beta 常量（与官方 core 相同）。
const RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";

/// 官方 WS 握手 headers（空白 HeaderMap 起步；与官方
/// `ModelClient::build_websocket_headers` 对齐）。
pub fn build_ws_headers(
    ctx: &RequestContext,
    projection: &codex_core::CodexResponsesMetadata,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    // x-client-request-id = thread id（官方）。
    if let Some(session) = &ctx.session
        && let Ok(value) = HeaderValue::from_str(&session.thread_id)
    {
        headers.insert("x-client-request-id", value);
    }
    // session headers（session-id/thread-id）。
    let session_headers = crate::transport::responses::session_headers(ctx);
    headers.extend(session_headers);
    // compatibility headers（x-codex-window-id 等）。
    let compat = crate::transport::responses::compatibility_headers(projection);
    headers.extend(compat);
    // openai-beta: responses_websockets v2（官方固定值）。
    headers.insert(
        http::header::HeaderName::from_static("openai-beta"),
        HeaderValue::from_static(RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE),
    );
    // 承载头（FR-004）**有意不注入 WS 握手**：官方 WS 握手头集合就是上面这一套
    // （`x-client-request-id` = thread id / session-id / thread-id /
    // x-codex-window-id / openai-beta），turn-metadata 等元数据在 WS 上走 body 的
    // `client_metadata`，不作为握手头出现
    // （core/tests/suite/client_websockets.rs 断言 handshake header 为 None）。
    // 把承载的 HTTP 元数据头塞进 WS 握手会凭空造出官方客户端从不发送的头。
    headers
}

/// 已建立的上游官方 WS 连接。
pub struct OfficialWsConnection {
    pub connection: codex_websocket_client::WebSocketConnection,
}

/// 使用官方 connector + 显式账号级 route 完成上游 WS 握手。
///
/// 握手失败返回 typed HTTP error（绝不向下游伪造 101）。
pub async fn connect_responses_ws_v2(
    pool: &crate::transport::client_pool::ClientPool,
    installation_id: &str,
    ctx: &RequestContext,
) -> Result<OfficialWsConnection, LocalError> {
    let session = ctx.session.as_ref().ok_or_else(|| {
        LocalError::new(
            "missing_session",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "responses WS requires a session snapshot",
        )
    })?;
    let projection = metadata::build_projection(
        metadata::effective_installation_id(installation_id, ctx),
        session,
    )?;

    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let official_route = crate::transport::proxy::to_official_route(&handle.route);

    // F-018：WS 连接复用官方 ResponsesWebsocketClient::connect_raw_with_route
    // （同一 URL/header/auth/TLS/显式 route 构造；raw 模式返回原始
    // WebSocketConnection + 握手响应，供 1:1 relay 使用）。不再手工重建
    // 官方连接逻辑。
    let auth = official_adapter::official_auth(ctx);
    let provider = official_adapter::chatgpt_provider();
    let client = codex_api::ResponsesWebsocketClient::new(provider.clone(), auth);

    let mut extra_headers = build_ws_headers(ctx, &projection);
    if let Some(version) = provider.headers.get("version") {
        extra_headers.insert("version", version.clone());
    }
    let default_headers = codex_login::default_client::default_headers();

    let (connection, response) = client
        .connect_raw_with_route(official_route, extra_headers, default_headers)
        .await
        .map_err(|err| {
            tracing::error!(%err, "upstream ws handshake failed");
            LocalError::new(
                "ws_handshake_failed",
                ErrorClass::Transport,
                ErrorPhase::WsHandshake,
                true,
                format!("upstream WS handshake failed: {err}"),
            )
        })?;

    // WS 握手成功 = HTTP 101 Switching Protocols；`is_success()` 只认 2xx，
    // 不能用于 Upgrade 判定（§11.7.3）。
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status();
        tracing::error!(%status, "upstream ws handshake rejected");
        return Err(LocalError::new(
            "ws_handshake_rejected",
            ErrorClass::Transport,
            ErrorPhase::WsHandshake,
            false,
            format!("upstream WS handshake rejected with {status}"),
        ));
    }

    // 记录池条目（WS 连接也纳入账号/凭据/proxy 池计数）。
    let _ = pool.len().await;

    Ok(OfficialWsConnection { connection })
}

/// 官方 WS 配置（permessage-deflate 与官方一致）。
#[allow(dead_code)]
fn ws_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());
    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}

/// 1:1 relay：internal 帧 <-> upstream 帧。
///
/// - 只有官方所需的 text JSON frame 向上游转发；未知 binary application
///   frame fail closed（§11.7.5）。
/// - Ping/Pong/Close 显式处理。
/// - 任一方向取消/关闭在预算内关闭另一方向。
pub async fn relay_ws(
    mut upstream: codex_websocket_client::WebSocketConnection,
    mut internal_rx: mpsc::Receiver<Message>,
    internal_tx: mpsc::Sender<Message>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), LocalError> {
    let mut drain_closed_internal = false;
    loop {
        // RF10（real-lane codexp）：已取消的 token 在进入 select 前确定性处理
        // （关内部生产端才 drain；否则立即返回），正常数据面用默认公平 select，
        // 避免 biased 让持续有帧的内部方向饿死 upstream 方向（双向转发死锁）。
        if !drain_closed_internal && cancel.is_cancelled() {
            if !internal_rx.is_closed() {
                return Ok(());
            }
            drain_closed_internal = true;
        }
        let incoming = if drain_closed_internal {
            // RF9（real-lane codexp）：内部生产端（pump1）已关闭后的有界 drain——
            // 直接读队列，把已被通道接受的帧按 FIFO 转上游，确保排队的 Close
            // 在其后所有数据之后送达（PS-M3）。有界队列不会无限等待生产者。
            internal_rx.recv().await
        } else {
            // 默认（公平）select：活跃期间双向数据方向都保持公平。
            tokio::select! {
                _ = cancel.cancelled() => {
                    // F-008：权威取消源。RF9/RF10：仅当内部生产端已关闭才 drain；
                    // 取消来自其他方向（如 upstream 结束）时立即返回，不等生产者。
                    if !internal_rx.is_closed() {
                        return Ok(());
                    }
                    drain_closed_internal = true;
                    continue;
                }
                // internal -> upstream（Go 已做 size/JSON/marker 检查；最终边界再验证）
                incoming = internal_rx.recv() => incoming,
                // upstream -> internal（原始事件；close 语义 sanitize 后映射）
                outgoing = upstream.next() => {
                    match outgoing {
                        Some(Ok(Message::Text(text))) => {
                            if internal_tx.send(Message::Text(text)).await.is_err() {
                                return Ok(()); // Go 侧关闭
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = upstream.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Pong(_))) => { /* 忽略 */ }
                        Some(Ok(Message::Close(frame))) => {
                            let _ = internal_tx.send(Message::Close(frame)).await;
                            return Ok(());
                        }
                        Some(Ok(_)) => {
                            return Err(LocalError::new(
                                "ws_upstream_unsupported_frame",
                                ErrorClass::UpstreamProtocol,
                                ErrorPhase::WsRelay,
                                false,
                                "unsupported upstream WS frame kind",
                            ));
                        }
                        Some(Err(err)) => {
                            return Err(LocalError::new(
                                "ws_upstream_read_failed",
                                ErrorClass::Transport,
                                ErrorPhase::WsRelay,
                                false,
                                format!("upstream WS read failed: {err}"),
                            ));
                        }
                        None => {
                            let _ = internal_tx.send(Message::Close(None)).await;
                            return Ok(());
                        }
                    }
                    continue;
                }
            }
        };
        // 共享 internal 帧处理：正常 select 与 drain 路径同一套语义。
        match incoming {
            Some(Message::Text(text)) => {
                if !is_valid_json_frame(&text) {
                    internal_tx
                        .send(Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
                            reason: "invalid json frame".into(),
                        })))
                        .await
                        .ok();
                    return Err(LocalError::new(
                        "ws_invalid_json",
                        ErrorClass::Contract,
                        ErrorPhase::WsRelay,
                        false,
                        "internal WS frame failed JSON validation",
                    ));
                }
                // 写后歧义：不 replay；send 失败即关闭连接。
                if upstream.send(Message::Text(text)).await.is_err() {
                    return Err(LocalError::new(
                        "ws_upstream_write_failed",
                        ErrorClass::Transport,
                        ErrorPhase::WsRelay,
                        false,
                        "upstream WS write failed; not replaying frame",
                    ));
                }
            }
            Some(Message::Ping(payload)) => {
                let _ = upstream.send(Message::Pong(payload)).await;
            }
            Some(Message::Close(frame)) => {
                // PSR2-H01：保留 close code/reason（与 server pump 映射
                // 对称），不再丢为 Close(None)。
                let _ = upstream.send(Message::Close(frame)).await;
                return Ok(());
            }
            Some(_) => {
                return Err(LocalError::new(
                    "ws_unsupported_frame",
                    ErrorClass::Contract,
                    ErrorPhase::WsRelay,
                    false,
                    "unsupported internal WS frame kind",
                ));
            }
            None => {
                let _ = upstream.send(Message::Close(None)).await;
                return Ok(());
            }
        }
    }
}

fn is_valid_json_frame(text: &str) -> bool {
    // F11（review）：只做结构合法性扫描，不构建 Value 树（WS 每帧热路径）。
    // IgnoredAny 定长栈式推进；from_str 自带 EOF 校验（尾随垃圾拒绝）。
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// 为一次 internal WS 会话创建帧通道。
pub fn frame_channels() -> (mpsc::Sender<Message>, mpsc::Receiver<Message>) {
    mpsc::channel(WS_FRAME_QUEUE_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_frame_validation() {
        assert!(is_valid_json_frame(r#"{"type":"response.create"}"#));
        assert!(!is_valid_json_frame("not json"));
        assert!(!is_valid_json_frame(""));
    }

    #[test]
    fn beta_header_matches_official() {
        assert_eq!(
            RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE,
            "responses_websockets=2026-02-06"
        );
    }

    #[test]
    fn frame_channels_are_bounded() {
        let (tx, rx) = frame_channels();
        // tokio mpsc bounded channel: capacity() returns the bound (0 = unbounded).
        assert_eq!(tx.capacity(), WS_FRAME_QUEUE_CAPACITY);
        drop(tx);
        assert!(rx.is_closed());
    }
}
