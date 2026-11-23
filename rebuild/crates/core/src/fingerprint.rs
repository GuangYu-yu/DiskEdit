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
}

impl Fingerprint {
    /// 由可稳定识别目标的材料构造。
    pub const fn strong(bytes: [u8; 32]) -> Self {
        Self {
            strength: Strength::Strong,
            bytes,
        }
    }

    /// 由不足以识别目标的材料构造。
    pub const fn unusable(bytes: [u8; 32]) -> Self {
        Self {
            strength: Strength::Unusable,
            bytes,
        }
    }

    pub const fn strength(self) -> Strength {
        self.strength
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
}
