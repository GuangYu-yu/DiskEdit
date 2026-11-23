//! 写入令牌：证明本步原像已经落盘。
//!
//! 令牌只能由 [`crate::app::execute::transaction`] 在记录写入成功之后铸造；
//! 其余模块拿到的是已有值，无法自行构造。写通道要求令牌，因此不存在"未登记
//! 就写目标"的调用形态。

use super::transaction::Mint;

/// 本步原像已落盘。
#[derive(Clone, Copy, Debug)]
pub struct Recorded {
    sequence: u64,
}

impl Recorded {
    /// 由事务模块铸造；`Mint` 只能在其定义模块内构造，因此别处无法仿造令牌。
    pub(crate) const fn mint(_mint: &Mint, sequence: u64) -> Self {
        Self { sequence }
    }

    /// 该令牌对应的帧序号。
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}
