//! UDS WebSocket upgrade 与 1:1 relay（TLS-CTD-001 §12.3 / §11.7）。

use axum::extract::State;
use axum::extract::ws::Message as AxumWsMessage;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::response::Response;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::contract::operation::Operation;
use crate::server::ServerState;
use crate::server::local_error_response;

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;

/// WS 并发连接计数（1:1，有界）。
static WS_CONNECTION_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) async fn handle_ws_upgrade(
    State(state): State<std::sync::Arc<ServerState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // WS 并发上限由 ws_slots 信号量权威保证（F-008 原子占位）；
    // 此处不再做 Relaxed 预检（竞态且冗余）。WS_CONNECTION_COUNT 仅
    // 保留为观测计数。
    let operation = Operation::ResponsesWebSocketV2;
    let (request_id, _account_hash) = crate::server::extract_request_meta(&headers);
    let request_id = request_id.unwrap_or_else(|| "unknown".to_string());
    // F7（review）：WS upgrade 请求头与 HTTP 路径一致地受数量/字节上限约束
    // （§16.2 顺序 3），避免超大头部绕过 HTTP 侧限制。
    if headers.len() > crate::limits::MAX_HEADER_COUNT
        || headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
            .sum::<usize>()
            > crate::limits::MAX_HEADER_BYTES
    {
        return local_error_response(
            &crate::contract::error::LocalError::new(
                "header_limits_exceeded",
                crate::contract::error::ErrorClass::Contract,
                crate::contract::error::ErrorPhase::Validate,
                false,
                "header limits exceeded",
            ),
            &request_id,
        );
    }
    let ctx = match crate::contract::headers::parse_request_context(&headers, operation) {
        Ok(ctx) => ctx,
        Err(err) => return local_error_response(&err, &request_id),
    };

    // RF2（real-lane codexp）：握手前就确定有效绝对 deadline（caller deadline
    // 或无则 3600s 硬上限），并施加到 admission、上游连接与整条 relay 生命周期——
    // 否则无 deadline 时手握手过程（DNS/TCP/proxy/TLS/upgrade）可在持锁情况下
    // 无限期占用 ws_slot。
    let effective_deadline = match ctx.deadline {
        Some(deadline) => tokio::time::Instant::from_std(deadline),
        None => tokio::time::Instant::now() + std::time::Duration::from_secs(3600),
    };

    // F-008：admission 原子占位（等待有界；先占位后连接，超限拒绝
    // 而不是事后计数；ws_slots 信号量是权威上限，无需预检计数）。
    // F7（review）：等待上限 = min(configured_wait, caller deadline remaining)，
    // 与 HTTP 路径一致，避免携带紧 deadline 的 WS 调用方白等满窗口。
    // RF4/RF6（real-lane codexp）：configured_wait 必须采用 caller 的
    // admission_timeout_ms（取 min(header, ADMISSION_WAIT_MS)），且当 caller
    // deadline 是更紧的预算时超时映射 operation_deadline_exceeded（不可重试的
    // deadline 语义）而非容量错误 admission_rejected——与 HTTP BudgetSource 对齐。
    let configured_wait = std::time::Duration::from_millis(
        ctx.admission_timeout_ms
            .unwrap_or(crate::limits::ADMISSION_WAIT_MS),
    )
    .min(std::time::Duration::from_millis(
        crate::limits::ADMISSION_WAIT_MS,
    ));
    let (admission_wait, deadline_limited) = match ctx.deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining <= configured_wait {
                (remaining, true)
            } else {
                (configured_wait, false)
            }
        }
        None => (configured_wait, false),
    };
    let ws_permit =
        match tokio::time::timeout(admission_wait, state.ws_slots.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Err(_) if deadline_limited => {
                return local_error_response(
                    &crate::contract::error::LocalError::new(
                        "operation_deadline_exceeded",
                        crate::contract::error::ErrorClass::Timeout,
                        crate::contract::error::ErrorPhase::WaitHeaders,
                        true,
                        "ws admission exceeded caller deadline",
                    ),
                    &request_id,
                );
            }
            _ => {
                return local_error_response(
                    &crate::contract::error::codes::admission_rejected(),
                    &request_id,
                );
            }
        };

    // 握手成功前不制造假 101：先建立上游 WS，失败返回 typed HTTP error。
    // H3：上游连接受 caller deadline 约束（过期立即失败，不占资源）。
    let upstream_future = crate::transport::websocket::connect_responses_ws_v2(
        &state.pool,
        &state.installation_id,
        &ctx,
    );
    // RF2：无论是否有 caller deadline，握手都受 effective_deadline 约束，
    // 杜绝无 deadline 时 DNS/TCP/proxy/TLS/upgrade 卡住持锁占用 ws_slot。
    let upstream = match tokio::time::timeout_at(effective_deadline, upstream_future).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(err)) => {
            tracing::error!(code = %err.code, phase = err.phase.as_str(), "internal ws upstream connect failed");
            return local_error_response(&err, &request_id);
        }
        Err(_) => {
            let err = crate::contract::error::LocalError::new(
                "operation_deadline_exceeded",
                crate::contract::error::ErrorClass::Timeout,
                crate::contract::error::ErrorPhase::WsHandshake,
                true,
                "ws upstream connect exceeded deadline",
            );
            return local_error_response(&err, &request_id);
        }
    };

    WS_CONNECTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    tracing::info!("ws upgrade accepted, installing on_upgrade");
    ws.on_upgrade(move |socket| {
        tracing::info!("on_upgrade invoked");
        relay_ws_session(socket, upstream.connection, effective_deadline, ws_permit)
    })
}

async fn relay_ws_session(
    internal: WebSocket,
    upstream: codex_websocket_client::WebSocketConnection,
    deadline: tokio::time::Instant,
    _ws_permit: tokio::sync::OwnedSemaphorePermit,
) {
    // 双向两个通道：Go→Rust 帧进入 relay 转发 upstream；
    // Rust→Go 帧由下游 pump 写回 internal socket（1:1 relay，§11.7）。
    let (go_to_rust_tx, go_to_rust_rx) = crate::transport::websocket::frame_channels();
    let (rust_to_go_tx, rust_to_go_rx) = crate::transport::websocket::frame_channels();
    // downstream Ping → 回 Pong 通道（sink 在 pump2；F-008 不得转发上游）。
    let (pong_tx, pong_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
    // 下游不支持帧（Binary）→ 通知 pump2 向客户端发 Close(1003)（review F4）。
    let (close_tx, close_rx) = tokio::sync::mpsc::channel::<()>(1);
    // futures::StreamExt::split：返回 (sink, stream)。
    let (internal_tx_half, internal_rx_half) = internal.split();

    // pump 1：internal socket → go_to_rust（下游帧上行）。
    let internal_to_relay = async move {
        let mut internal_rx_half = internal_rx_half;
        tracing::info!("relay pump1 start");
        while let Some(message) = internal_rx_half.next().await {
            match message {
                Ok(AxumWsMessage::Text(text)) => {
                    // axum 的 Utf8Bytes 与 tungstenite 不是同一类型（F13 复验），
                    // 仍需经 String 中转。
                    let text: tokio_tungstenite::tungstenite::Utf8Bytes = text.to_string().into();
                    if go_to_rust_tx.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Ok(AxumWsMessage::Ping(payload)) => {
                    // F-008：downstream Ping 由 sidecar 直接回 Pong，
                    // 不转发 upstream（Pong 由 pump2 写回 internal socket）。
                    let _ = pong_tx.send(payload).await;
                }
                Ok(AxumWsMessage::Pong(_)) => {
                    // RFC 6455 5.5.2：downstream Pong 是合法控制帧，直接忽略，
                    // 绝不因此拆除会话（review F4）。
                }
                Ok(AxumWsMessage::Binary(_)) => {
                    // 不支持的 binary 业务帧 fail closed：先经 pump2 向客户端
                    // 发 Close(1003)，再 teardown（review F4）。
                    tracing::warn!("unsupported downstream binary ws frame -> close(1003)");
                    let _ = close_tx.send(()).await;
                    break;
                }
                Ok(AxumWsMessage::Close(frame)) => {
                    // PS-M3：保留 downstream close code/reason（与 pump2 对称，
                    // 不再丢为 Close(None)）。
                    let upstream = frame.map(map_axum_close_to_tungstenite);
                    let _ = go_to_rust_tx.send(Message::Close(upstream)).await;
                    break;
                }
                Err(_) => break,
            }
        }
        tracing::info!("relay pump1 end");
    };

    // pump 2：rust_to_go → internal socket（上游事件下行）+ Pong 回写 + Close 通知。
    let relay_to_internal = relay_to_internal(internal_tx_half, rust_to_go_rx, pong_rx, close_rx);

    // relay：go_to_rust 帧 → upstream；upstream 事件 → rust_to_go。
    // F-008：token 传入 relay_ws 作为权威取消源——任一方向结束先 cancel，
    // relay 停止泵（不再依赖 select! drop 的偶然语义）。
    let cancel = tokio_util::sync::CancellationToken::new();
    let relay_task = {
        let cancel = cancel.clone();
        async move {
            let result = crate::transport::websocket::relay_ws(
                upstream,
                go_to_rust_rx,
                rust_to_go_tx,
                cancel,
            )
            .await;
            // H3：relay 错误必须可见（不得静默丢弃）。
            if let Err(err) = result {
                tracing::warn!(code = %err.code, phase = err.phase.as_str(), "ws relay ended with error");
            }
        }
    };

    // R4-H3：JoinSet 管理各方向任务——首终态出现 → cancel → 有界 grace
    // 等待剩余任务退出 → 超时 abort（不再直接 drop sibling future）。
    // F3/RF2（review）：整条会话受握手阶段确定的同一绝对 deadline 约束
    // （caller deadline 或无则 3600s 硬上限），防止挂起客户端永久占用
    // ws_slot + 上游连接。
    let session_deadline = deadline;
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(internal_to_relay);
    tasks.spawn(relay_to_internal);
    tasks.spawn(relay_task);

    let orchestrate = async {
        let _first = tasks.join_next().await;
        cancel.cancel();

        const WS_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
        let drain = async {
            while let Some(result) = tasks.join_next().await {
                if let Err(err) = result {
                    tracing::warn!("ws relay task failed: {err}");
                }
            }
        };
        if tokio::time::timeout(WS_SHUTDOWN_GRACE, drain)
            .await
            .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            tracing::warn!("ws relay tasks forced abort after grace");
        }
    };
    let cancel_deadline = cancel.clone();
    if tokio::time::timeout_at(session_deadline, orchestrate)
        .await
        .is_err()
    {
        cancel_deadline.cancel();
        // orchestrate future 被 drop → JoinSet 被 drop → 各任务被 abort
        // （internal socket 随之关闭），ws_slot 由 _ws_permit drop 释放。
        tracing::warn!("ws relay session ended by deadline (caller deadline or hard cap)");
    }
    tracing::info!("relay session end");
    WS_CONNECTION_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    // ws_permit 在函数返回（连接结束）时 Drop 释放。
}

async fn relay_to_internal<S>(
    mut internal_tx_half: S,
    mut rust_to_go_rx: mpsc::Receiver<Message>,
    mut pong_rx: mpsc::Receiver<bytes::Bytes>,
    mut close_rx: mpsc::Receiver<()>,
) where
    S: futures::Sink<AxumWsMessage> + Unpin,
{
    use futures::SinkExt;

    loop {
        tokio::select! {
            message = rust_to_go_rx.recv() => {
                let Some(message) = message else { break };
                let out = match message {
                    Message::Text(text) => AxumWsMessage::Text(text.to_string().into()),
                    Message::Ping(payload) => AxumWsMessage::Ping(payload),
                    Message::Pong(payload) => AxumWsMessage::Pong(payload),
                    Message::Close(frame) => {
                        let _ = internal_tx_half
                            .send(AxumWsMessage::Close(frame.map(map_tungstenite_close_to_axum)))
                            .await;
                        break;
                    }
                    Message::Binary(_) => {
                        // 上游 binary 业务帧 fail closed（§11.7.5）：关闭连接。
                        let _ = internal_tx_half
                            .send(AxumWsMessage::Close(Some(axum::extract::ws::CloseFrame {
                                code: 1008, // CloseCode::Policy
                                reason: "unsupported upstream frame".into(),
                            })))
                            .await;
                        break;
                    }
                    Message::Frame(_) => break,
                };
                if internal_tx_half.send(out).await.is_err() {
                    break;
                }
            }
            payload = pong_rx.recv() => {
                let Some(payload) = payload else { break };
                if internal_tx_half.send(AxumWsMessage::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Some(()) = close_rx.recv() => {
                // RF3（real-lane codexp）：只对显式的 unsupported-Binary 信号发
                // Close(1003)；close_tx 在 pump1 正常退出（Close/EOF/错误）时被
                // drop → recv() 返回 None，此处不匹配 → 走正常 teardown，不再
                // 误发 1003 覆盖对端 close code/reason。
                let _ = internal_tx_half
                    .send(AxumWsMessage::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1003, // CloseCode::Unsupported
                        reason: "unsupported downstream frame".into(),
                    })))
                    .await;
                break;
            }
        }
    }
}

#[allow(dead_code)]
fn _mpsc_probe(_tx: mpsc::Sender<Message>) {}

/// PS-M3：downstream(axum) close frame → upstream(tungstenite) close frame。
/// reason 按 RFC 6455 123-byte 上限做 UTF-8 安全截断。
pub(crate) fn map_axum_close_to_tungstenite(
    f: axum::extract::ws::CloseFrame,
) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(f.code),
        reason: truncate_close_reason(f.reason.as_str()).into(),
    }
}

/// PS-M3：upstream(tungstenite) close frame → downstream(axum) close frame。
pub(crate) fn map_tungstenite_close_to_axum(
    f: tokio_tungstenite::tungstenite::protocol::CloseFrame,
) -> axum::extract::ws::CloseFrame {
    axum::extract::ws::CloseFrame {
        code: u16::from(f.code),
        reason: truncate_close_reason(f.reason.as_str()).into(),
    }
}

/// RFC 6455 §5.5.1：close reason 上限 123 bytes，按 UTF-8 字符边界截断。
pub(crate) fn truncate_close_reason(reason: &str) -> &str {
    const MAX: usize = 123;
    if reason.len() <= MAX {
        return reason;
    }
    let mut end = MAX;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

#[cfg(test)]
mod close_reason_tests {
    use super::*;

    #[test]
    fn axum_close_preserves_code_and_reason() {
        let f = axum::extract::ws::CloseFrame {
            code: 1001,
            reason: "going away".into(),
        };
        let up = map_axum_close_to_tungstenite(f);
        assert_eq!(u16::from(up.code), 1001);
        assert_eq!(up.reason.as_str(), "going away");
    }

    #[test]
    fn tungstenite_close_preserves_code_and_reason() {
        let f = tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(1008u16),
            reason: "policy".into(),
        };
        let down = map_tungstenite_close_to_axum(f);
        assert_eq!(down.code, 1008);
        assert_eq!(down.reason.as_str(), "policy");
    }

    #[test]
    fn long_reason_truncated_at_utf8_boundary() {
        // 124 字节 ASCII → 截断为 123。
        let s = "a".repeat(124);
        assert_eq!(truncate_close_reason(&s).len(), 123);
        // 多字节字符:122 字节 ASCII + 1 个 3 字节中文 = 125 > 123,
        // 截断后不落在字符中间。
        let multi = format!("{}{}", "a".repeat(122), "中");
        let t = truncate_close_reason(&multi);
        assert!(t.len() <= 123);
        assert!(t.is_char_boundary(t.len()));
        assert_eq!(t, "a".repeat(122));
    }
}
