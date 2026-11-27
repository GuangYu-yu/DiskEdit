//! 几何：扇区尺寸校验、单位换算与对齐。

pub mod align;
pub mod sector;
pub mod units;

pub use sector::SectorSize;

/// 几何计算失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryError {
    /// 扇区尺寸为零。
    ZeroSectorSize,
    /// 扇区尺寸不在允许集合内。
    UnsupportedSectorSize(u64),
    /// 对齐值不是 2 的幂。
    MisalignedPowerOfTwo(u64),
    /// 计算越过 `u64` 上界。
    Overflow,
}
