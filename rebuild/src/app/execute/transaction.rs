//! 事务：把"原像已落盘"这一事实铸造成写入令牌的唯一来源。
//!
//! 令牌类型本身不可在别处构造；只有本模块能给出 [`Mint`]，因此不存在"未登记
//! 就写目标"的调用形态。具体事务步骤在 S5 起接入，这里固定铸造边界。

/// 写入令牌的铸造权。
///
/// 元组字段私有，模块外无法构造；持有者才能铸造 [`Recorded`]。
///
/// [`Recorded`]: super::token::Recorded
pub struct Mint(());

impl Mint {
    /// 由事务入口铸造；S5 接线事务后即被调用。
    #[allow(dead_code)]
    pub(crate) const fn new() -> Self {
        Self(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::execute::Recorded;

    #[test]
    fn mint_grants_recorded_tokens() {
        let mint = Mint::new();
        let token = Recorded::mint(&mint, 7);
        assert_eq!(token.sequence(), 7);
    }
}
