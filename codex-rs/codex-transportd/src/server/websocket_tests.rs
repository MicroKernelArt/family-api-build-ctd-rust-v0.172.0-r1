use std::time::Duration;

#[tokio::test]
async fn pump2_exits_when_pong_channel_closes() {
    let (_rust_to_go_tx, rust_to_go_rx) = crate::transport::websocket::frame_channels();
    let (pong_tx, pong_rx) = tokio::sync::mpsc::channel(1);
    let (_close_tx, close_rx) = tokio::sync::mpsc::channel(1);
    drop(pong_tx);

    tokio::time::timeout(
        Duration::from_millis(100),
        super::relay_to_internal(futures::sink::drain(), rust_to_go_rx, pong_rx, close_rx),
    )
    .await
    .expect("pump2 should exit when its pong channel closes");
}
