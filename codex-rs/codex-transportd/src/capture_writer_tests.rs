//! writer 状态机 + 诚实终态测试（确定性：不经 OS 线程，直接驱动
//! process_event / process_received）。

use super::*;
use crate::capture_writer::process_event;
use crate::capture_writer::process_received;
use crate::contract::RequestPurpose;
use crate::contract::headers::SessionSnapshot;
use std::collections::HashSet as HS;
use std::os::fd::{AsRawFd, OwnedFd};

const SESSION_DIR: &str = "0198f2b0-0000-7000-8000-000000000001";

struct TestRig {
    _dir: tempfile::TempDir,
    root_fd: OwnedFd,
    metrics: Arc<Metrics>,
    budget: Arc<ByteBudget>,
    tx: mpsc::Sender<CaptureEvent>,
    rx_path: std::sync::Mutex<Option<mpsc::Receiver<CaptureEvent>>>,
    whitelist: HashSet<String>,
}

impl TestRig {
    fn new(queue: usize) -> Self {
        Self::with_budget(queue, usize::MAX)
    }

    fn with_budget(queue: usize, max_total_bytes: usize) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_fd = crate::capture_writer::open_root(dir.path()).expect("open root");
        let (tx, rx) = mpsc::channel(queue);
        Self {
            _dir: dir,
            root_fd,
            metrics: Arc::new(Metrics::default()),
            budget: ByteBudget::arc(max_total_bytes),
            tx,
            rx_path: std::sync::Mutex::new(Some(rx)),
            whitelist: {
                let mut s = HS::new();
                s.insert("3".to_string());
                s
            },
        }
    }

    fn capture(&self) -> Capture {
        Capture {
            whitelist: self.whitelist.clone(),
            tx: self.tx.clone(),
            metrics: Arc::clone(&self.metrics),
            budget: Arc::clone(&self.budget),
        }
    }

    /// 收割当前队列中所有事件并同步处理（模拟 writer 消费，含 F2 预算
    /// 释放）；失败状态随事件走（R2-F1 共享信号），无需跨 drain 保留。
    fn drain(&self) {
        let rx = self.rx_path.lock().unwrap().take();
        if let Some(mut rx) = rx {
            while let Ok(ev) = rx.try_recv() {
                process_received(self.root_fd.as_raw_fd(), &self.metrics, &self.budget, ev);
            }
            // 放回以便后续 drain 复用。
            *self.rx_path.lock().unwrap() = Some(rx);
        }
    }

    /// capture root 的真实路径（受控文件系统转换的预置/拆除入口）。
    fn root_path(&self) -> &std::path::Path {
        self._dir.path()
    }

    fn session_path(&self, name: &str) -> PathBuf {
        self._dir.path().join("u3").join(SESSION_DIR).join(name)
    }

    fn entries(&self) -> Vec<String> {
        std::fs::read_dir(self._dir.path().join("u3").join(SESSION_DIR))
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.session_path(name)).ok()
    }
}

fn handle_for(cap: &Capture) -> CaptureHandle {
    cap.begin(&RequestContext {
        request_id: "req_capture_1".to_string(),
        user_id: Some("3".to_string()),
        session: Some(SessionSnapshot {
            session_id: SESSION_DIR.to_string(),
            thread_id: "t".to_string(),
            window_id: "w".to_string(),
        }),
        purpose: RequestPurpose::Request,
        ..Default::default()
    })
    .expect("handle")
}

#[test]
fn request_and_stream_captured_byte_exact_in_order() {
    let rig = TestRig::new(64);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"REQUEST-BYTES-01"));
    for chunk in [Bytes::from_static(b"chunk-"), Bytes::from_static(b"two")] {
        h.send_chunk(&chunk);
    }
    h.finish(true);
    drop(h);
    rig.drain();

    // request id 里的下划线在白名单内，prefix = <nanos>_req_capture_1。
    let entries = rig.entries();
    let req_name = entries.iter().find(|n| n.ends_with(".req")).expect(".req");
    let resp_name = entries
        .iter()
        .find(|n| n.ends_with(".resp"))
        .expect(".resp");
    assert!(req_name.ends_with("_req_capture_1.req"));
    assert_eq!(
        std::fs::read(rig.session_path(req_name)).unwrap(),
        b"REQUEST-BYTES-01"
    );
    assert_eq!(
        std::fs::read(rig.session_path(resp_name)).unwrap(),
        b"chunk-two",
        "SSE bytes must preserve order and byte equality"
    );
    assert!(!resp_name.ends_with(".resp.trunc") && !resp_name.ends_with(".resp.err"));
}

#[test]
fn overflow_marks_truncated_and_never_blocks() {
    // 队列容量 2；writer 消费由 drain() 显式驱动（确定性）。
    let rig = TestRig::new(2);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"R"));
    rig.drain(); // writer 消费请求事件，队列清空
    h.send_chunk(&Bytes::from_static(b"c1")); // 1/2
    h.send_chunk(&Bytes::from_static(b"c2")); // 2/2
    let t0 = std::time::Instant::now();
    h.send_chunk(&Bytes::from_static(b"c3")); // 满 → 丢弃 + truncated
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "try_send must never block the request path"
    );
    assert!(h.inner.truncated.load(Ordering::Relaxed));
    assert!(rig.metrics.capture_truncated_total.load(Ordering::Relaxed) >= 1);
    assert!(rig.metrics.capture_dropped_total.load(Ordering::Relaxed) >= 1);
    rig.drain(); // 写入 c1/c2
    h.finish(true); // Done 入队（终态 = trunc）
    rig.drain();
    // 溢出 → 终态 .resp.trunc（绝不伪装完整），已入队 chunk 保留。
    let entries = rig.entries();
    let trunc = entries
        .iter()
        .find(|n| n.ends_with(".resp.trunc"))
        .expect("overflow must end as .resp.trunc");
    assert_eq!(
        rig.read(trunc).unwrap(),
        b"c1c2",
        "queued chunks stay byte-true inside the truncated file"
    );
    assert!(!entries.iter().any(|n| n.ends_with(".resp.part")));
}

#[test]
fn lost_done_event_leaves_honest_counter_only() {
    // AM4-T3 第 2 类（终态事件本身被丢）：无 .resp* 文件、无误导，
    // 失败事实 = 计数器 + warn。
    let rig = TestRig::new(1);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"R"));
    rig.drain(); // 队列空
    h.send_chunk(&Bytes::from_static(b"c1")); // 占满队列
    h.finish(true); // 队列满 → Done 丢失
    rig.drain(); // 只消费 c1 → .resp.part 残留，无终态
    let entries = rig.entries();
    assert!(
        !entries.iter().any(|n| n.ends_with(".resp")
            || n.ends_with(".resp.trunc")
            || n.ends_with(".resp.err")),
        "lost Done must not fabricate a terminal state, got {entries:?}"
    );
    assert!(entries.iter().any(|n| n.ends_with(".resp.part")));
    assert!(rig.metrics.capture_dropped_total.load(Ordering::Relaxed) >= 1);
}

#[test]
fn stream_error_lands_as_resp_err() {
    let rig = TestRig::new(64);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"R"));
    h.send_chunk(&Bytes::from_static(b"partial"));
    h.finish(false); // 上游/断连错误
    drop(h);
    rig.drain();
    let entries = rig.entries();
    assert!(entries.iter().any(|n| n.ends_with(".resp.err")));
    assert_eq!(
        rig.read(entries.iter().find(|n| n.ends_with(".resp.err")).unwrap())
            .unwrap(),
        b"partial"
    );
}

#[test]
fn dropped_handle_without_events_leaves_no_misleading_file() {
    let rig = TestRig::new(1);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"R")); // 占满队列
    let h2 = handle_for(&cap); // 第二条请求
    h2.send_request(&Bytes::from_static(b"X")); // 队列满 → 入队失败
    drop(h2); // Drop 兜底 Done 也失败 → 无任何文件
    rig.drain();
    // 第二条请求不得产生任何文件（第一条的 .req 已由 drain 落盘）。
    let entries = rig.entries();
    assert!(
        !entries.iter().any(|n| n.contains("resp")),
        "no misleading response files for fully-dropped handle"
    );
    assert_eq!(
        rig.metrics.capture_dropped_total.load(Ordering::Relaxed) >= 2,
        true
    );
}

#[test]
fn capture_stream_forwards_bytes_and_terminates_honestly() {
    // 正常结束：字节原样转发 + .resp 终态。
    let rig = TestRig::new(64);
    let cap = rig.capture();
    let h = handle_for(&cap);
    let inner = futures::stream::iter(vec![
        Ok::<_, io::Error>(Bytes::from_static(b"a")),
        Ok(Bytes::from_static(b"b")),
    ]);
    let mut streamed = CaptureStream::new(inner, h);
    // 手动 poll（不依赖 executor 的阻塞包装）。
    let mut out = Vec::new();
    let mut cx = futures::task::Context::from_waker(futures::task::noop_waker_ref());
    loop {
        match Pin::new(&mut streamed).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(b))) => out.extend_from_slice(&b),
            Poll::Ready(None) => break,
            Poll::Pending => unreachable!("iter stream never pends"),
            Poll::Ready(Some(Err(_))) => unreachable!(),
        }
    }
    drop(streamed);
    rig.drain();
    assert_eq!(out, b"ab", "tee must forward bytes verbatim");

    // 错误终止：Err 原样上抛 + .resp.err。
    let rig2 = TestRig::new(64);
    let cap2 = rig2.capture();
    let h2 = handle_for(&cap2);
    let inner2 = futures::stream::iter(vec![
        Ok::<_, io::Error>(Bytes::from_static(b"x")),
        Err(io::Error::other("upstream boom")),
    ]);
    let mut s2 = CaptureStream::new(inner2, h2);
    let mut cx2 = futures::task::Context::from_waker(futures::task::noop_waker_ref());
    let first = Pin::new(&mut s2).poll_next(&mut cx2);
    let second = Pin::new(&mut s2).poll_next(&mut cx2);
    assert!(matches!(first, Poll::Ready(Some(Ok(_)))));
    assert!(
        matches!(second, Poll::Ready(Some(Err(_)))),
        "error must forward"
    );
    drop(s2);
    rig2.drain();
    let entries = rig2.entries();
    assert!(entries.iter().any(|n| n.ends_with(".resp.err")));
}

#[test]
fn drop_midflight_lands_as_resp_err_or_part_residual() {
    // 断连场景 A：Done 事件成功入队（writer 正常）→ .resp.err。
    let rig = TestRig::new(64);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"R"));
    h.send_chunk(&Bytes::from_static(b"half"));
    let inner = futures::stream::pending::<io::Result<Bytes>>();
    let s = CaptureStream::new(inner, h);
    drop(s); // 断连：Drop → finish(false) 入队
    rig.drain();
    let entries = rig.entries();
    let err_file = entries
        .iter()
        .find(|n| n.ends_with(".resp.err"))
        .expect("disconnect must land as .resp.err");
    assert_eq!(rig.read(err_file).unwrap(), b"half");

    // 断连场景 B：Done 永远到不了 writer（writer 停滞）→ .resp.part 残留。
    let rig2 = TestRig::new(64);
    let cap2 = rig2.capture();
    let h2 = handle_for(&cap2);
    h2.send_request(&Bytes::from_static(b"R"));
    h2.send_chunk(&Bytes::from_static(b"half"));
    rig2.drain(); // writer 写入 .resp.part 后停滞（不再消费）
    let inner2 = futures::stream::pending::<io::Result<Bytes>>();
    let s2 = CaptureStream::new(inner2, h2);
    drop(s2); // Done 入队但无人消费
    let entries2 = rig2.entries();
    assert!(
        entries2.iter().any(|n| n.ends_with(".resp.part")),
        "stalled writer must leave .resp.part residual, got {entries2:?}"
    );
    assert!(
        !entries2
            .iter()
            .any(|n| n.ends_with(".resp.err") || n.ends_with(".resp")),
        "residual must never be renamed to a terminal state"
    );
}

#[test]
fn zero_chunk_success_produces_empty_resp() {
    let rig = TestRig::new(4);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"R"));
    h.finish(true); // 空 SSE 流：0 chunk
    drop(h);
    rig.drain();
    let entries = rig.entries();
    let resp = entries
        .iter()
        .find(|n| n.ends_with(".resp"))
        .expect("terminal");
    assert_eq!(
        std::fs::read(rig.session_path(resp)).unwrap(),
        b"",
        "complete-but-empty stream is byte-true empty"
    );
}

// ---------------------------------------------------------------------------
// F1/R2-F1：入队期失败 → 共享失败信号 → 终态非成功
// （writer 侧写失败的同身份确定性测试见 R2-F2 修复：受控文件系统转换）
// ---------------------------------------------------------------------------

#[test]
fn request_enqueue_failure_makes_done_non_success() {
    // F1/R2-F1：请求体事件入队失败 → 共享失败信号标记 → 同一 handle 的
    // 响应终态非成功；普通 .resp 不得暗示完整 req/resp 对。
    let rig = TestRig::new(1);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_chunk(&Bytes::from_static(b"c1")); // 占满队列（1/1）
    h.send_request(&Bytes::from_static(b"REQUEST")); // 入队失败 → failed
    assert!(
        h.inner.failed.is_failed(),
        "request drop must mark the shared failure signal"
    );
    rig.drain(); // c1 → .resp.part；队列腾空
    h.finish(true); // ok=true 被信号压为 false → Done 入队
    rig.drain();
    let entries = rig.entries();
    assert!(
        entries.iter().any(|n| n.ends_with(".resp.err")),
        "dropped request body must end as .resp.err, got {entries:?}"
    );
    assert!(
        !entries.iter().any(|n| n.ends_with(".req")),
        "dropped request body must not fabricate a .req"
    );
    assert!(
        !entries.iter().any(|n| n.ends_with(".resp")),
        "plain .resp is forbidden when .req was dropped"
    );
}

// ---------------------------------------------------------------------------
// R2-F2：writer 写失败 → 同一 capture 的 Done 强制非成功终态
// （失败事件与 Done 共享同一逻辑身份：同一 (uid, session, prefix) 三元组
// + 事件携带的同一共享失败信号；用受控文件系统转换制造确定性写失败，
// 绝不靠在失败事件与 Done 之间换路径制造错误）
// ---------------------------------------------------------------------------

#[test]
fn request_fs_failure_forces_done_to_resp_err() {
    // 受控转换：预置普通文件占用 u3 目录名 → Request 事件解析 uid 目录
    // 失败（写失败计数 + 共享信号标记）→ 移除障碍 → Done(ok=true,
    // truncated=false) 在处理点读到信号 → 强制 .resp.err，绝不伪装
    // 完整 .resp（R2-F1/R2-F2）。
    let rig = TestRig::new(16);
    let root = rig.root_fd.as_raw_fd();
    let sig = FailureSignal::arc();
    std::fs::write(rig.root_path().join("u3"), b"plain file, not a dir")
        .expect("plant u3 obstruction");
    process_event(
        root,
        &rig.metrics,
        CaptureEvent::Request {
            uid_dir: "u3".to_string(),
            session_dir: SESSION_DIR.to_string(),
            prefix: "p1".to_string(),
            bytes: Bytes::from_static(b"R"),
            failed: Arc::clone(&sig),
        },
    );
    assert!(
        rig.metrics
            .capture_write_failures_total
            .load(Ordering::Relaxed)
            >= 1,
        "fs failure must be counted"
    );
    assert!(
        sig.is_failed(),
        "writer must mark the shared failure signal on Request failure"
    );
    std::fs::remove_file(rig.root_path().join("u3")).expect("remove obstruction");
    process_event(
        root,
        &rig.metrics,
        CaptureEvent::Done {
            uid_dir: "u3".to_string(),
            session_dir: SESSION_DIR.to_string(),
            prefix: "p1".to_string(),
            ok: true,
            truncated: false,
            failed: Arc::clone(&sig),
        },
    );
    let entries = rig.entries();
    let err_file = entries
        .iter()
        .find(|n| n.ends_with(".resp.err"))
        .expect("Done after failed Request must land as .resp.err");
    assert_eq!(
        rig.read(err_file).unwrap(),
        b"",
        "forced error terminal is created empty when no chunk ever landed"
    );
    assert!(
        !entries.iter().any(|n| n.ends_with(".resp")),
        "plain .resp is forbidden after any writer failure"
    );
    // R2-F1：writer 处理完事件即释放信号引用——只剩测试自持的 1 份，
    // 不存在任何进程级失败集合残留。
    assert_eq!(
        Arc::strong_count(&sig),
        1,
        "writer must not retain the per-capture failure signal"
    );
}

#[test]
fn chunk_fs_failure_forces_done_to_resp_err_and_keeps_good_bytes() {
    // 同一逻辑身份（u3/<session>/p1）贯穿失败与终态：预置普通文件占用
    // session 目录名 → chunk 1 解析 session 目录失败 → 移除障碍 →
    // chunk 2 落 .resp.part → Done 强制 .resp.err 且完好字节保留。
    let rig = TestRig::new(16);
    let root = rig.root_fd.as_raw_fd();
    let sig = FailureSignal::arc();
    std::fs::create_dir_all(rig.root_path().join("u3")).expect("create u3");
    std::fs::write(
        rig.root_path().join("u3").join(SESSION_DIR),
        b"plain file, not a dir",
    )
    .expect("plant session obstruction");
    process_event(
        root,
        &rig.metrics,
        CaptureEvent::Chunk {
            uid_dir: "u3".to_string(),
            session_dir: SESSION_DIR.to_string(),
            prefix: "p1".to_string(),
            bytes: Bytes::from_static(b"lost"),
            failed: Arc::clone(&sig),
        },
    );
    assert!(
        rig.metrics
            .capture_write_failures_total
            .load(Ordering::Relaxed)
            >= 1
    );
    assert!(sig.is_failed());
    std::fs::remove_file(rig.root_path().join("u3").join(SESSION_DIR)).expect("remove obstruction");
    process_event(
        root,
        &rig.metrics,
        CaptureEvent::Chunk {
            uid_dir: "u3".to_string(),
            session_dir: SESSION_DIR.to_string(),
            prefix: "p1".to_string(),
            bytes: Bytes::from_static(b"kept"),
            failed: Arc::clone(&sig),
        },
    );
    process_event(
        root,
        &rig.metrics,
        CaptureEvent::Done {
            uid_dir: "u3".to_string(),
            session_dir: SESSION_DIR.to_string(),
            prefix: "p1".to_string(),
            ok: true,
            truncated: false,
            failed: Arc::clone(&sig),
        },
    );
    let entries = rig.entries();
    let err_name = entries
        .iter()
        .find(|n| n.ends_with(".resp.err"))
        .expect("failed chunk must force .resp.err");
    assert_eq!(
        rig.read(err_name).unwrap(),
        b"kept",
        "chunks that did land stay byte-true inside the honest error terminal"
    );
    assert!(!entries.iter().any(|n| n.ends_with(".resp")));
    assert_eq!(
        Arc::strong_count(&sig),
        1,
        "writer must not retain the per-capture failure signal"
    );
}

#[test]
fn writer_side_request_failure_flows_through_queue_to_forced_err() {
    // 全队列路径：Request 事件入队后由 writer 处理时写失败 → 共享信号
    // 标记 → 后续 Done 在处理点强制 .resp.err；请求体字节丢失 = 无 .req。
    let rig = TestRig::new(8);
    let cap = rig.capture();
    std::fs::write(rig.root_path().join("u3"), b"plain file, not a dir")
        .expect("plant u3 obstruction");
    let h = handle_for(&cap);
    h.send_request(&Bytes::from_static(b"REQUEST-BYTES"));
    rig.drain(); // Request 写失败：无 .req、write_failures+1、信号标记
    assert!(
        rig.metrics
            .capture_write_failures_total
            .load(Ordering::Relaxed)
            >= 1
    );
    std::fs::remove_file(rig.root_path().join("u3")).expect("remove obstruction");
    h.send_chunk(&Bytes::from_static(b"c1"));
    rig.drain(); // 障碍已除：c1 落 .resp.part
    h.finish(true);
    drop(h);
    rig.drain(); // Done 在处理点读信号 → 强制 .resp.err
    let entries = rig.entries();
    let err_name = entries
        .iter()
        .find(|n| n.ends_with(".resp.err"))
        .expect("writer-side failure must force .resp.err through the queue");
    assert_eq!(rig.read(err_name).unwrap(), b"c1");
    assert!(
        !entries.iter().any(|n| n.ends_with(".req")),
        "lost request body must not fabricate a .req"
    );
    assert!(!entries.iter().any(|n| n.ends_with(".resp")));
}

// ---------------------------------------------------------------------------
// F2：字节预算——预留/释放精确一次、拒绝诚实、溢出 fail closed
// ---------------------------------------------------------------------------

#[test]
fn byte_budget_reserves_and_releases_exactly_once() {
    let rig = TestRig::with_budget(16, 8);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_chunk(&Bytes::from_static(b"abc")); // 预留 3
    assert_eq!(rig.budget.outstanding(), 3);
    rig.drain(); // writer 处理 → 精确释放一次
    assert_eq!(rig.budget.outstanding(), 0);
    h.send_chunk(&Bytes::from_static(b"abcdefgh")); // 8 ≤ 8
    assert_eq!(rig.budget.outstanding(), 8);
    rig.drain();
    assert_eq!(rig.budget.outstanding(), 0);
    assert!(rig.metrics.capture_dropped_total.load(Ordering::Relaxed) == 0);
}

#[test]
fn failed_enqueue_rolls_back_budget_reservation() {
    let rig = TestRig::with_budget(1, usize::MAX);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_chunk(&Bytes::from_static(b"abc")); // 入队成功，预留 3
    h.send_chunk(&Bytes::from_static(b"de")); // 队列满 → 回滚，净 0
    assert_eq!(
        rig.budget.outstanding(),
        3,
        "failed enqueue must roll back its reservation"
    );
    assert!(h.inner.truncated.load(Ordering::Relaxed));
    rig.drain();
    assert_eq!(rig.budget.outstanding(), 0);
}

#[test]
fn budget_rejection_matches_queue_rejection_honesty() {
    let rig = TestRig::with_budget(16, 4);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_chunk(&Bytes::from_static(b"abc")); // 3/4
    h.send_chunk(&Bytes::from_static(b"de")); // 3+2 > 4 → 拒绝
    assert!(h.inner.truncated.load(Ordering::Relaxed));
    assert!(rig.metrics.capture_truncated_total.load(Ordering::Relaxed) >= 1);
    assert!(rig.metrics.capture_dropped_total.load(Ordering::Relaxed) >= 1);
    h.finish(true); // truncated → 终态 .resp.trunc
    rig.drain();
    let entries = rig.entries();
    let trunc = entries
        .iter()
        .find(|n| n.ends_with(".resp.trunc"))
        .expect("budget rejection must end as .resp.trunc (same as queue rejection)");
    assert_eq!(rig.read(trunc).unwrap(), b"abc");
    assert!(!entries.iter().any(|n| n.ends_with(".resp")));
}

#[test]
fn request_budget_rejection_makes_done_non_success() {
    let rig = TestRig::with_budget(16, 8);
    let cap = rig.capture();
    let h = handle_for(&cap);
    h.send_request(&Bytes::from(vec![b'x'; 16])); // 16 > 8 → 拒绝
    assert!(h.inner.failed.is_failed());
    assert!(
        !h.inner.truncated.load(Ordering::Relaxed),
        "request rejection marks failure, not truncation"
    );
    h.finish(true);
    rig.drain();
    let entries = rig.entries();
    assert!(entries.iter().any(|n| n.ends_with(".resp.err")));
    assert!(!entries.iter().any(|n| n.ends_with(".req")));
    assert!(!entries.iter().any(|n| n.ends_with(".resp")));
}

#[test]
fn budget_overflow_fails_closed_without_panic() {
    let rig = TestRig::with_budget(16, usize::MAX);
    let cap = rig.capture();
    let h = handle_for(&cap);
    // 经真实预留路径构造临界态（R3-F1）：占住 MAX-2 → 剩 2 字节余量，
    // 随后请求 4 字节 → checked_add 溢出。全程不直写原子字段。
    assert!(rig.budget.try_reserve(usize::MAX - 2));
    assert_eq!(rig.budget.outstanding(), usize::MAX - 2);
    h.send_chunk(&Bytes::from_static(b"abcd")); // 溢出 → 拒绝（fail closed）
    assert!(rig.metrics.capture_dropped_total.load(Ordering::Relaxed) >= 1);
    assert_eq!(rig.budget.outstanding(), usize::MAX - 2);
    h.send_chunk(&Bytes::from_static(b"ab")); // MAX-2+2 = MAX ≤ MAX → 成功
    assert_eq!(rig.budget.outstanding(), usize::MAX);
    rig.drain(); // checked release（R2-F3）：MAX ≥ 2 → 精确递减，不 panic、不清零
    assert_eq!(rig.budget.outstanding(), usize::MAX - 2);
}

#[test]
fn byte_budget_over_release_fails_closed_and_is_observable() {
    // R2-F3：不可能的超额释放 → 计数器保持不变（fail closed，不吞其他
    // 在途预留、不虚降水位）+ 返回 false 供观测/测试；零长度释放是
    // no-op 成功。全程无任何载荷数据参与。
    let budget = ByteBudget::new(16);
    assert!(
        !budget.release(4),
        "over-release on zero outstanding must be reported"
    );
    assert_eq!(budget.outstanding(), 0);
    assert!(budget.try_reserve(10));
    assert!(
        !budget.release(11),
        "release larger than outstanding must fail closed"
    );
    assert_eq!(
        budget.outstanding(),
        10,
        "real reservations must survive an impossible over-release"
    );
    assert!(budget.release(10));
    assert_eq!(budget.outstanding(), 0);
    assert!(budget.release(0), "zero-length release is a no-op success");
    assert_eq!(budget.outstanding(), 0);
}
