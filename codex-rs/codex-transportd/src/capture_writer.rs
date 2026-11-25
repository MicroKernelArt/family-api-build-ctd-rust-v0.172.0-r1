//! capture writer：专用 OS 线程 + fd 锚定系统调用（capture 的私有兄弟模块）。
//!
//! 从 capture.rs 拆出（AM4 模块尺寸纪律）；只被 capture 模块消费。
//! 诚实终态状态机（F1/R2-F1）：任何 Request/Chunk 事件在 writer 侧写失败
//! 后，标记该事件携带的 per-capture 共享失败信号；同一 capture 的后续
//! Done 在处理点读取同一信号并强制非成功终态（`.resp.err`），绝不伪装
//! 完整 `.resp`；错误终态自身失败时保留 `.resp.part` 残留。
//! 有界性（R2-F1 根因消除）：writer 不维护任何以 capture 身份为键的
//! 集合——失败状态只存在于事件携带的共享信号里，随 handle 及其在途
//! 事件消亡；Done 被队列丢弃也不再留下 writer 常驻状态。
//! 预算记账（F2）：writer 每收到并处理一个携带字节的事件，精确释放
//! 一次预留；入队失败的事件从未到达 writer，由入队方回滚。

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use crate::capture::CaptureEvent;
use crate::capture_budget::ByteBudget;
use crate::observability::Metrics;

fn cstr(name: &str) -> io::Result<CString> {
    Ok(CString::new(name)?)
}

pub(crate) fn open_root(root: &std::path::Path) -> io::Result<OwnedFd> {
    // root 由部署配置（受信）给出；仍拒绝 symlink，并要求是真目录。
    let c = cstr(root.to_string_lossy().as_ref())?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// fd 锚定打开子目录（O_NOFOLLOW：预先植入的 symlink 一律拒绝）。
fn open_dir_at(parent: RawFd, name: &str) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            parent,
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    Err(io::Error::last_os_error())
}

/// fd 锚定创建子目录：mkdirat + O_NOFOLLOW 打开验证。绝不用
/// openat O_CREAT|O_DIRECTORY 建目录（部分内核该组合产出普通文件）。
/// 已存在时仅当为真目录才可继续；符号链接/普通文件一律拒绝。
fn create_dir_at(parent: RawFd, name: &str) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    let rc = unsafe { libc::mkdirat(parent, c.as_ptr(), 0o700) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    open_dir_at(parent, name)
}

fn create_file_at(dir: RawFd, name: &str) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            dir,
            c.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn append_file_at(dir: RawFd, name: &str) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            dir,
            c.as_ptr(),
            libc::O_CREAT | libc::O_APPEND | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn write_all(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            // write(2)==0 携带非空输入：绝不自旋（AM4），诚实报错。
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "capture write made no progress",
            ));
        }
        data = &data[n as usize..];
    }
    Ok(())
}

fn rename_at(dir: RawFd, from: &str, to: &str) -> io::Result<()> {
    let from = cstr(from)?;
    let to = cstr(to)?;
    let rc = unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 解析（必要时 fd 锚定创建）u<uid>/<session> 目录链，返回 session dirfd。
fn resolve_session_dir(root: RawFd, uid_dir: &str, session_dir: &str) -> io::Result<OwnedFd> {
    let uid_fd = create_dir_at(root, uid_dir)?;
    let session_fd = create_dir_at(uid_fd.as_raw_fd(), session_dir)?;
    Ok(session_fd)
}

/// per-capture 共享失败信号（R2-F1）：请求/响应链路上任何一处失败——
/// handle 侧入队/预算拒绝（capture.rs）或 writer 侧文件系统写失败——都
/// 标记同一实例；同一 capture 的 Done 在 writer 处理点读取它并强制
/// `.resp.err`。
///
/// 有界性（R2-F1 根因消除）：信号由该 capture 的事件携带（Arc 克隆），
/// 生命周期 = handle + 其在途事件；writer 处理完事件即释放引用。不存在
/// 任何以 capture 身份为键的进程级集合，Done 被丢弃也不再留下 writer
/// 常驻状态——诚实事实只由文件残留（`.resp.part`）+ 计数器承载。
///
/// 内存序：mark 用 Release、is_failed 用 Acquire——writer 线程在 Done
/// 处理点必须观察到入队侧/handle 线程此前的失败标记；信号只承载一个
/// bool，无其他数据需要同步。
#[derive(Debug, Default)]
pub(crate) struct FailureSignal {
    failed: AtomicBool,
}

impl FailureSignal {
    /// 同一 capture 的所有事件共享一个信号实例（capture.rs begin() 创建）。
    pub(crate) fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 标记失败（幂等；任意线程）。Release 配对 is_failed 的 Acquire。
    pub(crate) fn mark(&self) {
        self.failed.store(true, Ordering::Release);
    }

    /// 读取失败标记。Acquire 配对 mark 的 Release（跨线程可见性）。
    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// 单事件处理（writer 线程执行；纯 fd 锚定，无任何 pathname 操作）。
/// 独立成函数便于对状态机做确定性单测（AM2-T2/AM3-T3/F1/R2-F1）。
pub(crate) fn process_event(root: RawFd, metrics: &Metrics, ev: CaptureEvent) {
    // Done 在处理点检查共享失败信号：mpsc FIFO 保证此前同 capture 的
    // Request/Chunk 已被处理，其写失败已标记信号（R2-F1）。
    let force_err = matches!(ev, CaptureEvent::Done { .. }) && ev.failed().is_failed();
    let result = process_event_inner(root, &ev, force_err);
    if let Err(err) = result {
        metrics
            .capture_write_failures_total
            .fetch_add(1, Ordering::Relaxed);
        match &ev {
            CaptureEvent::Request { .. } | CaptureEvent::Chunk { .. } => {
                // 标记该 capture 的共享信号：后续 Done 强制 `.resp.err`；
                // 事件被丢弃时信号随之消亡，无任何 writer 集合残留（R2-F1）。
                ev.failed().mark();
                tracing::warn!(
                    error = %err,
                    "capture write failure; this capture's Done lands as .resp.err"
                );
            }
            CaptureEvent::Done { .. } => {
                // 终态化自身失败：`.resp.part` 残留即诚实事实。
                tracing::warn!(error = %err, "capture finalize failure; file state stays honest");
            }
        }
    }
}

fn process_event_inner(root: RawFd, ev: &CaptureEvent, force_err: bool) -> io::Result<()> {
    match ev {
        CaptureEvent::Request {
            uid_dir,
            session_dir,
            prefix,
            bytes,
            ..
        } => {
            let dir = resolve_session_dir(root, uid_dir, session_dir)?;
            // 请求体经 .req.part + renameat：裸 .req 必然字节完整（AM2-T2）。
            let part = format!("{prefix}.req.part");
            {
                let fd = create_file_at(dir.as_raw_fd(), &part)?;
                write_all(fd.as_raw_fd(), bytes)?;
            } // close before rename（fd drop）
            rename_at(dir.as_raw_fd(), &part, &format!("{prefix}.req"))
        }
        CaptureEvent::Chunk {
            uid_dir,
            session_dir,
            prefix,
            bytes,
            ..
        } => {
            let dir = resolve_session_dir(root, uid_dir, session_dir)?;
            let fd = append_file_at(dir.as_raw_fd(), &format!("{prefix}.resp.part"))?;
            write_all(fd.as_raw_fd(), bytes)
        }
        CaptureEvent::Done {
            uid_dir,
            session_dir,
            prefix,
            ok,
            truncated,
            ..
        } => {
            let dir = resolve_session_dir(root, uid_dir, session_dir)?;
            let part = format!("{prefix}.resp.part");
            // F1：该 capture 此前任何 Request/Chunk 写失败 → 终态强制
            // `.resp.err`；之后即使错误终态化自身失败，也只剩 `.resp.part`
            // 残留——两种结局都不伪装完整。
            let terminal = if force_err {
                format!("{prefix}.resp.err")
            } else {
                match (*ok, *truncated) {
                    (true, false) => format!("{prefix}.resp"),
                    (true, true) => format!("{prefix}.resp.trunc"),
                    _ => format!("{prefix}.resp.err"),
                }
            };
            match rename_at(dir.as_raw_fd(), &part, &terminal) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // 没有任何 chunk 到过 writer：产出一个空的终端文件，
                    // 仍然诚实表达该终态（0 字节 + 后缀即事实）。
                    let fd = create_file_at(dir.as_raw_fd(), &terminal)?;
                    drop(fd);
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
    }
}

/// writer 主循环消费单位：处理事件 + 精确释放一次字节预留（F2）。
/// spawn_writer 与测试 drain 共用，保证释放语义只有一份实现。
pub(crate) fn process_received(
    root: RawFd,
    metrics: &Metrics,
    budget: &ByteBudget,
    ev: CaptureEvent,
) {
    let carried = ev.byte_len();
    process_event(root, metrics, ev);
    if !budget.release(carried) {
        // 保守记账（R2-F3）：正确路径不可能超额释放；水位保持不变并
        // 单条 warn（无任何载荷数据）。
        tracing::warn!("capture byte budget over-release; accounting left unchanged");
    }
}

pub(crate) fn spawn_writer(
    rx: mpsc::Receiver<CaptureEvent>,
    root: OwnedFd,
    metrics: Arc<Metrics>,
    budget: Arc<ByteBudget>,
) {
    std::thread::Builder::new()
        .name("capture-writer".to_string())
        .spawn(move || {
            // 专用线程：blocking_recv + 同步 fs 系统调用，绝不占 executor。
            let mut rx = rx;
            while let Some(ev) = rx.blocking_recv() {
                process_received(root.as_raw_fd(), &metrics, &budget, ev);
            }
        })
        .expect("spawn capture writer thread");
}

#[cfg(test)]
#[path = "capture_writer_syscall_tests.rs"]
mod tests;
