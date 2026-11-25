//! 测试专用环境变量保护（R4-M2）：进程级 env 变异必须串行且 RAII 恢复，
//! 避免并行测试数据竞态与污染。

use std::sync::Mutex;

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// 获取环境变异锁；drop 时由调用方负责恢复旧值（guard 只保证串行）。
pub struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    key: &'static str,
    old: Option<String>,
}

/// 加锁并保存指定 key 的旧值（无则 None）；测试结束后调用 restore()。
pub fn env_guard_for(key: &'static str) -> EnvGuard {
    let lock = ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let old = std::env::var(key).ok();
    EnvGuard {
        _lock: lock,
        key,
        old,
    }
}

impl EnvGuard {
    /// 恢复 key 的旧值（无旧值则删除）。
    pub fn restore(&self) {
        match &self.old {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// 无 key 的通用锁（调用方自行管理 set_var/remove_var）。
pub fn env_guard() -> EnvGuard {
    env_guard_for("__CTD_ENV_GUARD_UNUSED__")
}
