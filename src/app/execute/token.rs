//! 写入令牌：证明本步原像已经落盘。
//!
//! 令牌只能由 [`crate::app::execute::transaction`] 在记录写入成功（或回滚时
//! 从已有记录装载原像）之后铸造；其余模块拿到的是已有值，无法自行构造。写
//! 通道要求令牌，因此不存在"未登记就写目标"的调用形态。
//!
//! 令牌绑定它所属的记录会话：写通道在构造时记住本会话，收到别的会话铸造的
//! 令牌即拒绝。令牌不实现 `Copy`/`Clone`，可以复制再跨步复用的形态不可表达。

use std::sync::atomic::{AtomicU64, Ordering};

use super::transaction::Mint;

/// 一次记录事务的会话标识。
///
/// 每次打开记录目录都会得到新值；它不落盘，只用于把令牌约束在本次会话内。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(u64);

impl SessionId {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// 本步原像已落盘。
#[derive(Debug)]
pub struct Recorded {
    session: SessionId,
    sequence: u64,
}

impl Recorded {
    /// 由事务模块铸造；`Mint` 只能在其定义模块内构造，因此别处无法仿造令牌。
    pub(crate) const fn mint(mint: &Mint, sequence: u64) -> Self {
        Self {
            session: mint.session(),
            sequence,
        }
    }

    /// 令牌所属的会话。
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// 该令牌对应的帧序号。
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}
