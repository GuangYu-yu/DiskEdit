//! 目标指纹与其可比较等级。
//!
//! 指纹只回答"这两次观测是不是同一个目标"。只有 [`Strength::Strong`] 的指纹
//! 才能用于写入与记录身份；不可用等级不构成身份，比较一律不成立。

/// 指纹的可比较等级。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    /// 可稳定识别同一目标。
    Strong,
    /// 不足以识别目标。
    Unusable,
}

/// 目标的稳定指纹：等级 + 32 字节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    strength: Strength,
    bytes: [u8; 32],
    /// 身份来自用户声明的名字而非设备事实；只有它在设备自身证明不了身份时才会出现。
    declared: bool,
}

impl Fingerprint {
    /// 由可稳定识别目标的材料构造。
    pub const fn strong(bytes: [u8; 32]) -> Self {
        Self {
            strength: Strength::Strong,
            bytes,
            declared: false,
        }
    }

    /// 由不足以识别目标的材料构造。
    pub const fn unusable(bytes: [u8; 32]) -> Self {
        Self {
            strength: Strength::Unusable,
            bytes,
            declared: false,
        }
    }

    /// 由用户声明的名字派生指纹。
    ///
    /// 这是命名空间的键，不是设备事实：同一名字在两次调用之间稳定地指同一目标，
    /// 换个名字就找不到原有记录——与「同一载体必须用同一个名字续跑」的契约一致。
    /// 只用于自动身份不可得的目标；能被设备自身稳定识别的目标不使用它。
    pub fn declared(name: &str) -> Self {
        // 32 字节由四个不同种子的 FNV-1a 展开；算法固定、跨版本稳定，仅供相等比较。
        let mut bytes = [0u8; 32];
        for (index, chunk) in bytes.chunks_mut(8).enumerate() {
            let seed = 0xcbf2_9ce4_8422_2325u64
                .wrapping_add((index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            chunk.copy_from_slice(&fnv1a64_seeded(name.as_bytes(), seed).to_le_bytes());
        }
        Self {
            strength: Strength::Strong,
            bytes,
            declared: true,
        }
    }

    pub const fn strength(self) -> Strength {
        self.strength
    }

    /// 身份是否只是用户声明的名字：这样的身份无法再用设备事实核对。
    pub const fn is_declared(self) -> bool {
        self.declared
    }

    pub const fn bytes(self) -> [u8; 32] {
        self.bytes
    }

    /// 两个指纹是否指向同一目标；任一方不可比较时结论为否。
    pub fn matches(self, other: Self) -> bool {
        self.strength == Strength::Strong
            && other.strength == Strength::Strong
            && self.bytes == other.bytes
    }
}

/// 带种子的 FNV-1a 64，用于把声明名稳定展开成指纹字节。
fn fnv1a64_seeded(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_strong_fingerprints_compare() {
        let bytes = [7u8; 32];
        let strong = Fingerprint::strong(bytes);
        assert!(strong.matches(strong));
        assert!(!strong.matches(Fingerprint::unusable(bytes)));
        assert!(!Fingerprint::unusable(bytes).matches(Fingerprint::unusable(bytes)));

        let mut other = [7u8; 32];
        other[0] = 8;
        assert!(!strong.matches(Fingerprint::strong(other)));
    }

    #[test]
    fn bytes_and_strength_are_preserved() {
        let bytes = [0x5au8; 32];
        let strong = Fingerprint::strong(bytes);
        assert_eq!(strong.bytes(), bytes);
        assert_eq!(strong.strength(), Strength::Strong);
        assert_eq!(Fingerprint::unusable(bytes).strength(), Strength::Unusable);
    }

    #[test]
    fn declared_names_are_stable_and_distinct() {
        let first = Fingerprint::declared("export-a");
        let again = Fingerprint::declared("export-a");
        assert_eq!(first, again);
        assert!(first.matches(again));
        assert!(!first.matches(Fingerprint::declared("export-b")));
        assert_ne!(first.bytes(), [0u8; 32], "展开结果不能退化成全零");
        // 声明名与设备事实是两种来源：只有前者允许"无法再用设备核对"。
        assert!(first.is_declared());
        assert!(!Fingerprint::strong([0x42; 32]).is_declared());
        assert!(!Fingerprint::unusable([0; 32]).is_declared());
    }
}
