//! 测试用固定种子输入发生器：可重复、无外部依赖。
//!
//! 只服务 `cfg(test)`。生成策略不是"均匀随机"：调用方把边界值（0、1、最大整数、
//! 扇区边界）放进取值池，由本模块负责组合与挑选，随机只用于覆盖组合空间。
//! 每个用例把 seed 与轮次写进断言消息，失败即可按同一 seed 复现。

pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        // seed 直接成为状态初值；0 与任何值的序列都不同，无需特判
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        // splitmix64：短、无状态依赖、跨平台位宽一致
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn u64(&mut self) -> u64 {
        self.next_u64()
    }

    pub(crate) fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// `[0, n)`；n 为 0 时返回 0（调用方传空池是编程错误，不在此处 panic 兜底）
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub(crate) fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    pub(crate) fn pick<T: Copy>(&mut self, pool: &[T]) -> T {
        pool[self.index(pool.len())]
    }
}
