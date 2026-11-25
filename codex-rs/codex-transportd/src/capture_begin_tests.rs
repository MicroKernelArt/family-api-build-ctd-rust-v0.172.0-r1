//! begin() 过滤矩阵测试（不依赖进程 env / 全局单例）。

use super::*;
use crate::contract::RequestPurpose;
use crate::contract::headers::SessionSnapshot;
use std::collections::HashSet as HS;

fn ctx(uid: Option<&str>, session: Option<&str>, request_id: &str) -> RequestContext {
    RequestContext {
        request_id: request_id.to_string(),
        user_id: uid.map(ToString::to_string),
        session: session.map(|sid| SessionSnapshot {
            session_id: sid.to_string(),
            thread_id: "t".to_string(),
            window_id: "w".to_string(),
        }),
        purpose: RequestPurpose::Request,
        ..Default::default()
    }
}

fn capture(queue: usize) -> (Arc<Capture>, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let (tx, rx) = mpsc::channel::<CaptureEvent>(queue);
    // begin 过滤矩阵只看过滤逻辑：receiver 交给 mem::forget 常驻，
    // 队列保持可用；不启动 writer 线程。
    std::mem::forget(rx);
    let mut whitelist = HS::new();
    whitelist.insert("3".to_string());
    (
        Arc::new(Capture {
            whitelist,
            tx,
            metrics: Arc::new(Metrics::default()),
            budget: ByteBudget::arc(usize::MAX),
        }),
        root.path().to_path_buf(),
    )
}

#[test]
fn begin_none_without_whitelist_match() {
    let (cap, _root) = capture(16);
    // 非白名单用户。
    assert!(cap.begin(&ctx(Some("9"), Some("s1"), "r1")).is_none());
    // 无 user header。
    assert!(cap.begin(&ctx(None, Some("s1"), "r1")).is_none());
}

#[test]
fn begin_none_on_unsanitizable_components() {
    let (cap, _root) = capture(16);
    // request id 越界（>64 或含非法字符）→ 跳过 capture。
    assert!(cap.begin(&ctx(Some("3"), Some("s1"), "../etc")).is_none());
    // user id 非法 → 跳过。
    assert!(cap.begin(&ctx(Some("../etc"), Some("s1"), "r1")).is_none());
}

#[test]
fn begin_ok_and_invalid_session_bucket() {
    let (cap, _root) = capture(16);
    let h = cap.begin(&ctx(
        Some("3"),
        Some("0198f2b0-0000-7000-8000-000000000001"),
        "r1",
    ));
    assert!(h.is_some());
    // session 消毒失败 → `_invalid` 兜底桶。
    let h = cap.begin(&ctx(Some("3"), Some("../../etc"), "r1"));
    assert!(h.is_some());
    assert_eq!(h.unwrap().inner.session_dir, "_invalid");
}
