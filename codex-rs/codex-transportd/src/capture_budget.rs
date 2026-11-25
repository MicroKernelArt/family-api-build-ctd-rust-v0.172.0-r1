//! capture 进程内共享字节预算（F2/R2-F3）：从 capture.rs 拆出（AM4 模块
//! 尺寸纪律）；被 capture（handle 入队侧）与 capture_writer（writer 消费
//! 侧）共同消费。
//!
//! 预算记账（F2）：入队前 `try_reserve`，writer 收到并处理事件后
//! `release` 恰好一次，入队失败由入队方 `release` 回滚。预留侧算术
//! 溢出/超预算一律拒绝（fail closed）；释放侧（R2-F3）保守 checked——
//! 不可能的超额释放不修改计数器并返回 false 供观测/测试，绝不饱和
//! 清零、绝不吞掉其他在途预留。绝不阻塞、绝不重试。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 队列在途字节预算（F2/R2-F3）。
#[derive(Debug)]
pub(crate) struct ByteBudget {
    max: usize,
    /// 在途字节数；私有记账状态（R3-F1）：临界态只能经 try_reserve/release
    /// 行为构造，外部只读观测走 outstanding()。
    outstanding: AtomicUsize,
}

impl ByteBudget {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            max,
            outstanding: AtomicUsize::new(0),
        }
    }

    pub(crate) fn arc(max: usize) -> Arc<Self> {
        Arc::new(Self::new(max))
    }

    /// 当前在途字节数（观测/测试）。
    pub(crate) fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Relaxed)
    }

    /// 预留 len 字节；超预算或 checked_add 溢出 → false（fail closed）。
    pub(crate) fn try_reserve(&self, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let mut cur = self.outstanding.load(Ordering::Relaxed);
        loop {
            let Some(next) = cur.checked_add(len) else {
                return false;
            };
            if next > self.max {
                return false;
            }
            match self.outstanding.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                // Ok(payload) 携带 next 本身；预算已精确预留在上面 checked_add。
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// 释放 len 字节（writer 处理后/入队失败回滚）。保守 checked 记账
    /// （R2-F3）：超额释放只可能来自记账 bug——此时计数器保持不变
    /// （fail closed：不吞掉其他在途预留、不虚降水位），返回 false 供
    /// 调用方观测/测试；正常路径返回 true。零长度释放是 no-op 成功。
    pub(crate) fn release(&self, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let mut released = true;
        let _ = self
            .outstanding
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                if v < len {
                    released = false; // 超额释放：放弃本次更新
                    None
                } else {
                    Some(v - len)
                }
            });
        released
    }
}
