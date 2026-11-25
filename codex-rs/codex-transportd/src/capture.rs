//! capture 模块（capture-r1）：白名单用户请求/响应字节旁路落盘。
//!
//! 纪律（G5/G6/AM3-T1/AM3-T5/AM4-T2..T6/F1/F2/F4）：
//! - 转发热路径只做非阻塞 `try_send`（一次/请求体 + 一次/SSE chunk +
//!   一次/流终止），零同步 I/O、零重试循环；请求体只克隆 `Bytes` 句柄，
//!   绝不整包拷贝（F2）；
//! - 队列双重有界：事件数（CAPTURE_QUEUE，硬上限 MAX_QUEUE）+ 在途字节
//!   预算（CAPTURE_MAX_BYTES）：入队前预留、writer 收到事件后精确释放
//!   一次、入队失败回滚；溢出 fail closed（F2）；
//! - 全部文件系统系统调用收敛在专用 writer OS 线程（`blocking_recv` 消费，
//!   实现见私有兄弟模块 `capture_writer`），绝不占用 tokio executor worker；
//! - fd 锚定 I/O：writer 持 capture root 的 dirfd，其下一切目录/文件操作走
//!   `openat`（O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC / O_CREAT|O_EXCL）与
//!   `renameat`，root 之下不存在任何 pathname 操作，无法被符号链接重定向；
//! - 诚实终态：`.req` 仅经 `.req.part` rename 产生（裸 `.req` 必然字节完整）；
//!   响应终态 rename 到 `.resp`（完整）|`.resp.trunc`（溢出截断）|`.resp.err`
//!   （流出错/断连/请求体丢失/任何 writer 写失败——F1）；事件在入队前就
//!   丢失（队列满/预算拒绝/关闭、句柄先行 drop）时不产生误导性文件，
//!   失败事实 = 计数器 + 单条 warn（带 request_id）；请求体事件入队失败
//!   会使该 handle 的响应终态非成功（F1）；
//! - 失败可见性有界（R2-F1）：入队期失败与 writer 侧写失败标记同一
//!   per-capture 共享信号（事件携带 Arc，随 handle/在途事件消亡）；
//!   writer 侧不存在以 capture 身份为键的进程级集合；
//! - 预算双重封顶（R2-F3）：CAPTURE_QUEUE ≤ MAX_QUEUE 且
//!   CAPTURE_MAX_BYTES ≤ MAX_TOTAL_BYTES；env 解析封顶归一化（显式且
//!   被测试），程序化配置由 Capture::new 直接拒绝（InvalidInput）；
//!   超额释放 fail closed（水位不变、可观测），预算水位永不虚降；
//! - 本模块禁止引用 upstream/projection 相关代码（G6 静态检查红线）。

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

use crate::capture_budget::ByteBudget;
use crate::capture_writer::FailureSignal;
use crate::contract::RequestContext;
use crate::observability::Metrics;

/// CAPTURE_QUEUE 缺省值（事件数；bounded，防无界内存）。
pub const DEFAULT_QUEUE: usize = 4096;
/// CAPTURE_QUEUE 硬上限（事件数）：配置值超过即封顶（F2）。
pub const MAX_QUEUE: usize = 65536;
/// CAPTURE_MAX_BYTES 缺省值（64 MiB，字节）：保守的队列在途字节预算——
/// 覆盖数百条典型 300KB 级事件在途，超出即拒绝新事件（fail closed），
/// 不依赖磁盘/内存余量兜底（F2）。
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// CAPTURE_MAX_BYTES 硬上限（R2-F3）：env 解析与程序化配置一律不得超过。
/// 在途字节是进程常驻内存（writer OS 线程消费前的排队事件）；512 MiB =
/// 8× 缺省，足以让 MAX_QUEUE 硬上限内的典型小 chunk 流量全速入队，同时
/// 为配置失误兜底。env 超限 → 封顶（归一化）；程序化超限 → InvalidInput。
pub const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
/// 路径组件白名单最大长度（G3）。
const MAX_COMPONENT_LEN: usize = 64;

/// 严格路径组件消毒：仅 [A-Za-z0-9_-]，长度 1..=64；其余一律拒绝。
/// 客户端可控 header 值（session/request id）最终进文件系统路径，
/// 必须先过此函数；失败方落到 `_invalid` 兜底桶或整体跳过 capture。
pub fn sanitize(s: &str) -> Option<String> {
    if s.is_empty() || s.len() > MAX_COMPONENT_LEN {
        return None;
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Some(s.to_string())
    } else {
        None
    }
}

/// CAPTURE_QUEUE 解析（纯函数便于确定性测试）：缺省/非法/0 → 缺省值；
/// 超上限 → 硬封顶 MAX_QUEUE（F2）。
fn parse_queue(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|q| *q > 0)
        .map_or(DEFAULT_QUEUE, |q| q.min(MAX_QUEUE))
}

/// CAPTURE_MAX_BYTES 解析（纯函数便于确定性测试）：缺省/非法/0 → 缺省值；
/// 超硬上限 → 封顶 MAX_TOTAL_BYTES（R2-F3：env 路径显式归一化并被测试
/// 覆盖；程序化配置则由 Capture::new 直接拒绝，不走归一化）。
fn parse_max_total_bytes(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .map_or(DEFAULT_MAX_TOTAL_BYTES, |v| v.min(MAX_TOTAL_BYTES))
}

/// capture 配置（compose 注入 env；缺省空 = 功能关闭，零开销）。
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub root: PathBuf,
    pub whitelist: HashSet<String>,
    pub queue: usize,
    /// 队列在途字节预算（F2）：writer 未处理事件的字节总量上限。
    pub max_total_bytes: usize,
}

impl CaptureConfig {
    /// 从环境解析；CAPTURE_ROOT 缺省空 = None（功能关闭）。
    pub fn from_env() -> Option<Self> {
        let root = std::env::var("CAPTURE_ROOT").ok()?;
        if root.is_empty() {
            return None;
        }
        let whitelist: HashSet<String> = std::env::var("CAPTURE_USER_IDS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        Some(Self {
            root: PathBuf::from(root),
            whitelist,
            queue: parse_queue(std::env::var("CAPTURE_QUEUE").ok().as_deref()),
            max_total_bytes: parse_max_total_bytes(
                std::env::var("CAPTURE_MAX_BYTES").ok().as_deref(),
            ),
        })
    }
}

/// writer 事件：目录以"已消毒组件名"传递，绝不携带路径；
/// writer 在 root dirfd 之下 fd 锚定地解析/创建。每个事件携带其 capture
/// 的共享失败信号（R2-F1）：writer 侧写失败由此标记，Done 处理点由此
/// 读取——失败状态随事件走，无进程级集合。
/// 可见性：仅进程内 channel 传输（pub(crate)），避免把 crate 内部信号
/// 类型泄漏进公共 API。
pub(crate) enum CaptureEvent {
    Request {
        uid_dir: String,
        session_dir: String,
        prefix: String,
        bytes: Bytes,
        failed: Arc<FailureSignal>,
    },
    Chunk {
        uid_dir: String,
        session_dir: String,
        prefix: String,
        bytes: Bytes,
        failed: Arc<FailureSignal>,
    },
    Done {
        uid_dir: String,
        session_dir: String,
        prefix: String,
        ok: bool,
        truncated: bool,
        failed: Arc<FailureSignal>,
    },
}

impl CaptureEvent {
    /// 事件携带的字节数（预算记账；Done 为 0）。
    pub(crate) fn byte_len(&self) -> usize {
        match self {
            CaptureEvent::Request { bytes, .. } | CaptureEvent::Chunk { bytes, .. } => bytes.len(),
            CaptureEvent::Done { .. } => 0,
        }
    }

    /// 该 capture 的共享失败信号（R2-F1）：handle 侧与 writer 侧标记同一
    /// 实例，Done 处理点读取。
    pub(crate) fn failed(&self) -> &Arc<FailureSignal> {
        match self {
            CaptureEvent::Request { failed, .. }
            | CaptureEvent::Chunk { failed, .. }
            | CaptureEvent::Done { failed, .. } => failed,
        }
    }
}

/// 进程级 capture 单例；未启用 = None（G4 零开销路径）。
static CAPTURE: OnceLock<Arc<Capture>> = OnceLock::new();

pub struct Capture {
    whitelist: HashSet<String>,
    tx: mpsc::Sender<CaptureEvent>,
    metrics: Arc<Metrics>,
    budget: Arc<ByteBudget>,
}

impl Capture {
    /// 构造并启动专用 writer OS 线程（全部 fs 系统调用收敛于此线程）。
    /// 程序化配置一律先验证（R2-F3）：queue 0（rendezvous 通道，破坏
    /// 有界缓冲语义）或 > MAX_QUEUE、字节预算 0 或 > MAX_TOTAL_BYTES →
    /// InvalidInput；绝不 panic、绝不静默越界。验证先于 root 打开与
    /// channel 构造。env 路径的归一化输出（parse_queue /
    /// parse_max_total_bytes）恒在本验证域内。
    pub fn new(config: CaptureConfig, metrics: Arc<Metrics>) -> io::Result<Self> {
        let queue = config.queue;
        if queue == 0 || queue > MAX_QUEUE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("capture queue {queue} outside 1..={MAX_QUEUE}"),
            ));
        }
        let max_total_bytes = config.max_total_bytes;
        if max_total_bytes == 0 || max_total_bytes > MAX_TOTAL_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("capture max_total_bytes {max_total_bytes} outside 1..={MAX_TOTAL_BYTES}"),
            ));
        }
        let root_fd = crate::capture_writer::open_root(&config.root)?;
        let budget = ByteBudget::arc(config.max_total_bytes);
        let (tx, rx) = mpsc::channel(config.queue);
        crate::capture_writer::spawn_writer(rx, root_fd, Arc::clone(&metrics), Arc::clone(&budget));
        Ok(Self {
            whitelist: config.whitelist,
            tx,
            metrics,
            budget,
        })
    }

    /// 初始化进程级单例（serve() 启动时调用一次；重复 init 返回错误）。
    pub fn init(config: CaptureConfig, metrics: Arc<Metrics>) -> Result<(), String> {
        let capture = Self::new(config, metrics).map_err(|e| format!("capture init: {e}"))?;
        CAPTURE
            .set(Arc::new(capture))
            .map_err(|_| "capture already initialised".to_string())
    }

    pub fn dropped_total(&self) -> u64 {
        self.metrics.capture_dropped_total.load(Ordering::Relaxed)
    }

    pub fn truncated_total(&self) -> u64 {
        self.metrics.capture_truncated_total.load(Ordering::Relaxed)
    }

    pub fn write_failures_total(&self) -> u64 {
        self.metrics
            .capture_write_failures_total
            .load(Ordering::Relaxed)
    }

    /// 白名单 + 消毒过滤（G3/G4）。None = 不抓（调用方零开销）。
    pub fn begin(&self, ctx: &RequestContext) -> Option<CaptureHandle> {
        let uid = ctx.user_id.as_deref()?;
        if !self.whitelist.contains(uid) {
            return None;
        }
        let uid_clean = sanitize(uid)?;
        let uid_dir = format!("u{uid_clean}");
        // session-id 消毒失败 → `_invalid` 兜底桶（而非放弃整条请求）。
        let session_dir = ctx
            .session
            .as_ref()
            .map(|s| sanitize(&s.session_id).unwrap_or_else(|| "_invalid".to_string()))?;
        let request_id = sanitize(&ctx.request_id)?;
        let prefix = format!("{}_{}", unix_nanos(), request_id);
        Some(CaptureHandle {
            inner: Arc::new(HandleInner {
                uid_dir,
                session_dir,
                prefix,
                request_id: ctx.request_id.clone(),
                tx: self.tx.clone(),
                metrics: Arc::clone(&self.metrics),
                budget: Arc::clone(&self.budget),
                finished: AtomicBool::new(false),
                truncated: AtomicBool::new(false),
                failed: FailureSignal::arc(),
                warned: AtomicBool::new(false),
            }),
        })
    }
}

/// 进程级单例访问（未启用 = None）。
pub fn global() -> Option<&'static Arc<Capture>> {
    CAPTURE.get()
}

/// begin() 进程级入口；未启用/不匹配 → None。
pub fn begin(ctx: &RequestContext) -> Option<CaptureHandle> {
    global()?.begin(ctx)
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

struct HandleInner {
    uid_dir: String,
    session_dir: String,
    prefix: String,
    request_id: String,
    tx: mpsc::Sender<CaptureEvent>,
    metrics: Arc<Metrics>,
    budget: Arc<ByteBudget>,
    finished: AtomicBool,
    truncated: AtomicBool,
    /// 共享失败信号（F1/R2-F1）：入队期失败与 writer 侧写失败标记同一
    /// 实例（事件携带），使响应终态非成功；随 handle/在途事件消亡。
    failed: Arc<FailureSignal>,
    warned: AtomicBool,
}

impl HandleInner {
    fn record_drop(&self, stage: &str) {
        self.metrics
            .capture_dropped_total
            .fetch_add(1, Ordering::Relaxed);
        if !self.warned.swap(true, Ordering::Relaxed) {
            // 单条 warn（带 request_id，低敏）；不打印任何捕获内容。
            tracing::warn!(
                request_id = %self.request_id,
                stage = stage,
                "capture event dropped (queue full/closed); file state stays honest"
            );
        }
    }

    fn mark_truncated(&self) {
        self.truncated.store(true, Ordering::Relaxed);
        self.metrics
            .capture_truncated_total
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// 每请求 capture 句柄：仅非阻塞 try_send；Drop 时若尚未收尾，
/// 以 best-effort Done(ok=false) 兜底（诚实不完整，绝不伪装成功）。
pub struct CaptureHandle {
    inner: Arc<HandleInner>,
}

impl CaptureHandle {
    /// 请求体落盘事件（热路径一次 try_send；克隆 `Bytes` 句柄，零整包
    /// 拷贝，F2）。预算/队列拒绝 → 该 handle 响应终态非成功（F1）。
    pub fn send_request(&self, bytes: &Bytes) {
        let len = bytes.len();
        if !self.inner.budget.try_reserve(len) {
            self.inner.failed.mark();
            self.inner.record_drop("request");
            return;
        }
        let ev = CaptureEvent::Request {
            uid_dir: self.inner.uid_dir.clone(),
            session_dir: self.inner.session_dir.clone(),
            prefix: self.inner.prefix.clone(),
            bytes: bytes.clone(),
            failed: Arc::clone(&self.inner.failed),
        };
        if self.inner.tx.try_send(ev).is_err() {
            self.inner.budget.release(len);
            self.inner.failed.mark();
            self.inner.record_drop("request");
        }
    }

    /// SSE chunk 旁路（每 chunk 一次 try_send；原样放行，延迟 +0）。
    /// 预算/队列失败 → 标记 truncated（终态 .resp.trunc）+ 计数 + 单条 warn。
    pub fn send_chunk(&self, bytes: &Bytes) {
        let len = bytes.len();
        if !self.inner.budget.try_reserve(len) {
            self.inner.mark_truncated();
            self.inner.record_drop("chunk");
            return;
        }
        let ev = CaptureEvent::Chunk {
            uid_dir: self.inner.uid_dir.clone(),
            session_dir: self.inner.session_dir.clone(),
            prefix: self.inner.prefix.clone(),
            bytes: bytes.clone(),
            failed: Arc::clone(&self.inner.failed),
        };
        if self.inner.tx.try_send(ev).is_err() {
            self.inner.budget.release(len);
            self.inner.mark_truncated();
            self.inner.record_drop("chunk");
        }
    }

    /// 流正常结束：ok=true；流出错/断连：ok=false。Drop 兜底视为 !ok。
    /// handle 侧可见的入队期失败（F1）先折入 ok；writer 侧写失败由事件
    /// 携带的共享信号在 Done 处理点再查一次（R2-F1，双保险）。
    pub fn finish(&self, ok: bool) {
        if self.inner.finished.swap(true, Ordering::Relaxed) {
            return;
        }
        let ok = ok && !self.inner.failed.is_failed();
        let ev = CaptureEvent::Done {
            uid_dir: self.inner.uid_dir.clone(),
            session_dir: self.inner.session_dir.clone(),
            prefix: self.inner.prefix.clone(),
            ok,
            truncated: self.inner.truncated.load(Ordering::Relaxed),
            failed: Arc::clone(&self.inner.failed),
        };
        if self.inner.tx.try_send(ev).is_err() {
            self.inner.record_drop("done");
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // 断连/句柄先行 drop：best-effort 终态；没有任何事件到达 writer 时
        // 不产生误导性文件——失败事实由计数器 + warn 承载（AM4-T3）。
        self.finish(false);
    }
}

/// 响应流旁路包装：不改流类型（仍为 `Result<Bytes, io::Error>` 流），
/// 只旁路 try_send；Err/None/Drop 三类终止都映射到诚实终态（G2/INV3）。
pub struct CaptureStream<S> {
    inner: S,
    handle: CaptureHandle,
}

impl<S> CaptureStream<S> {
    pub fn new(inner: S, handle: CaptureHandle) -> Self {
        Self { inner, handle }
    }
}

impl<S> Stream for CaptureStream<S>
where
    S: Stream<Item = io::Result<Bytes>> + Unpin,
{
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = Pin::new(&mut self.inner);
        match inner.poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.handle.send_chunk(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.handle.finish(false);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                self.handle.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;

/// begin() 过滤矩阵测试（不依赖进程 env / 全局单例）。
#[cfg(test)]
#[path = "capture_begin_tests.rs"]
mod begin_tests;

/// writer 状态机 + 诚实终态测试（确定性：不经 OS 线程，直接驱动
/// process_event / process_received）。
#[cfg(test)]
#[path = "capture_writer_tests.rs"]
mod writer_tests;
