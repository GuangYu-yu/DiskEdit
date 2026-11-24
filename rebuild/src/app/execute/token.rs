//! 写入令牌：证明本步原像已经落盘。
//!
//! 令牌只能由 [`crate::app::execute::transaction`] 在记录写入成功（或回滚时
//! 从已有记录装载原像）之后铸造；其余模块拿到的是已有值，无法自行构造。写
//! 通道要求令牌，因此不存在"未登记就写目标"的调用形态。

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

    /// 来宾执行宿主委托的步骤。
    ///
    /// 宿主在发送步骤之前已经把原像落盘，工作进程拿到的是"原像已登记"这一事实
    /// 的委托，而不是自行登记的权利；它不读写记录目录。
    pub(crate) const fn delegated() -> Self {
        Self { sequence: 0 }
    }

    /// 该令牌对应的帧序号。
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}
