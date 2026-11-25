//! relay_ws 收敛回归测试（real-lane codexp RF9 / PS-M3）。
//!
//! `cancel_after_downstream_close_drains_full_frame_queue_in_fifo_order`：
//! 内部生产端（pump1）把 31 条 Text + 末尾 Close(1000,"done") 全部入队后退出、
//! token 已预先 cancel——relay 必须按 FIFO 把全部 Text 转发、Close 最后送达，
//! 不丢帧、不重排（此前 RF7 的 8 条扫描会丢帧且永远到不了 Close）。

use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use codex_transportd::transport::websocket::{frame_channels, relay_ws};

const CAP: usize = 32; // WS_FRAME_QUEUE_CAPACITY

async fn recorder(listener: tokio::net::TcpListener) -> Vec<Message> {
    let (stream, _) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .expect("ws accept");
    let mut frames = Vec::new();
    use futures::StreamExt as _;
    while let Some(frame) = ws.next().await {
        match frame {
            Ok(msg) => frames.push(msg),
            Err(_) => break,
        }
    }
    frames
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_after_downstream_close_drains_full_frame_queue_in_fifo_order() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let recorder_task = tokio::spawn(recorder(listener));

    // 客户端经公开 connector（显式 Direct route + 非 TLS 模式）建立 loopback 上游。
    let url = format!("ws://{addr}/");
    let request = url.into_client_request().expect("client request");
    let factory = codex_http_client::HttpClientFactory::new(
        codex_http_client::OutboundProxyPolicy::ReqwestDefault,
    );
    let connector = codex_websocket_client::WebSocketConnector::new_with_tls_mode(
        &factory,
        codex_websocket_client::WebSocketTlsMode::TungsteniteDefault,
    )
    .expect("connector");
    let (upstream, _response) = connector
        .connect_with_route(
            request,
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default(),
            codex_http_client::OutboundProxyRoute::Direct,
        )
        .await
        .expect("client handshake");

    // 内部通道：满队列前 31 条编号 Text + 末尾 Close(1000, "done")。
    let (internal_tx, internal_rx) = frame_channels();
    for i in 0..(CAP - 1) {
        internal_tx
            .send(Message::Text(format!(r#"{{"frame":"{i:03}"}}"#).into()))
            .await
            .expect("enqueue");
    }
    internal_tx
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        })))
        .await
        .expect("enqueue close");
    drop(internal_tx); // 唯一 sender 关闭 → rx.is_closed() = true

    // 预先 cancel：biased select 先命中 cancel 分支 → is_closed 门控进入 drain。
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();

    let relay_internal_tx = {
        let (tx, _rx) = frame_channels();
        tx
    };
    let relay_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        relay_ws(upstream, internal_rx, relay_internal_tx, cancel),
    )
    .await
    .expect("relay within grace");

    assert!(relay_result.is_ok(), "relay_ws should return Ok(())");

    let frames = recorder_task.await.expect("recorder finished");

    // 断言：31 条 Text 严格 FIFO，随后恰好一条 Close(1000, "done")。
    let texts: Vec<&str> = frames
        .iter()
        .filter_map(|m| match m {
            Message::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), CAP - 1, "all Text frames forwarded");
    for (i, t) in texts.iter().enumerate() {
        assert_eq!(
            *t,
            format!(r#"{{"frame":"{i:03}"}}"#),
            "FIFO order preserved"
        );
    }
    let closes: Vec<&CloseFrame> = frames
        .iter()
        .filter_map(|m| match m {
            Message::Close(Some(c)) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(closes.len(), 1, "exactly one terminal close");
    assert_eq!(closes[0].code, CloseCode::Normal);
    assert_eq!(closes[0].reason, "done");
    assert_eq!(frames.len(), CAP, "Text*31 + Close = 32 frames");
    assert!(
        matches!(frames.last(), Some(Message::Close(Some(_)))),
        "Close is the final frame"
    );
}

// SARF-0002 regression：正常数据面必须公平——持续有内部帧时，upstream 方向仍能被轮询，
// 不允许 biased 饿死 upstream（双向转发死锁）。
async fn recorder_with_upstream_text(listener: tokio::net::TcpListener) -> Vec<Message> {
    let (stream, _) = listener.accept().await.expect("accept");
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .expect("ws accept");
    use futures::SinkExt as _;
    // 握手后立即向客户端（relay 的 upstream）发一条 Text。
    let _ = ws
        .send(Message::Text(r#"{"kind":"upstream-ping"}"#.into()))
        .await;
    let mut frames = Vec::new();
    use futures::StreamExt as _;
    while let Some(frame) = ws.next().await {
        match frame {
            Ok(msg) => frames.push(msg),
            Err(_) => break,
        }
    }
    frames
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_internal_traffic_does_not_starve_upstream_frames() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let recorder_task = tokio::spawn(recorder_with_upstream_text(listener));

    let url = format!("ws://{addr}/");
    let request = url.into_client_request().expect("client request");
    let factory = codex_http_client::HttpClientFactory::new(
        codex_http_client::OutboundProxyPolicy::ReqwestDefault,
    );
    let connector = codex_websocket_client::WebSocketConnector::new_with_tls_mode(
        &factory,
        codex_websocket_client::WebSocketTlsMode::TungsteniteDefault,
    )
    .expect("connector");
    let (upstream, _response) = connector
        .connect_with_route(
            request,
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default(),
            codex_http_client::OutboundProxyRoute::Direct,
        )
        .await
        .expect("client handshake");

    let (internal_tx, internal_rx) = frame_channels();
    // relay 的 internal_tx（upstream→internal 方向）保留 receiver 以断言 upstream Text 到达。
    let (relay_internal_tx, mut relay_internal_rx) = frame_channels();

    // 生产者持续把内部帧塞满，保持 internal_rx 一直 ready。
    let producer = {
        let internal_tx = internal_tx.clone();
        tokio::spawn(async move {
            let mut i = 0u64;
            loop {
                if internal_tx
                    .send(Message::Text(format!(r#"{{"noise":"{i}"}}"#).into()))
                    .await
                    .is_err()
                {
                    break;
                }
                i += 1;
            }
        })
    };
    drop(internal_tx); // relay 侧唯一收端仍是 internal_rx（producer 持有 clone）

    let cancel = tokio_util::sync::CancellationToken::new();
    let relay_task = tokio::spawn(relay_ws(
        upstream,
        internal_rx,
        relay_internal_tx,
        cancel.clone(),
    ));

    // 有界时间内应收到 upstream 的 Text（内部持续压力下不被饿死）。
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), relay_internal_rx.recv())
        .await
        .expect("upstream text within timeout")
        .expect("channel open");
    assert_eq!(got, Message::Text(r#"{"kind":"upstream-ping"}"#.into()));

    producer.abort();
    cancel.cancel();
    let _ = relay_task.await;
    let _ = recorder_task.await;
}
