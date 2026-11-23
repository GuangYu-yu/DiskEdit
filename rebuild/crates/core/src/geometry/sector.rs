//! 扇区尺寸与其上的 LBA 换算。
//!
//! 允许集合固定为 512 起、每次翻倍至 65536；其它取值一律拒绝，
//! 以免下游把"任意整数"当作扇区尺寸使用。

use super::GeometryError;

/// 已校验的扇区尺寸，单位字节。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SectorSize(u64);

impl SectorSize {
    /// 允许的扇区尺寸集合。
    pub const ALLOWED: [u64; 8] = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

    pub fn new(bytes: u64) -> Result<Self, GeometryError> {
        if bytes == 0 {
            return Err(GeometryError::ZeroSectorSize);
        }
        if Self::ALLOWED.contains(&bytes) {
            Ok(Self(bytes))
        } else {
            Err(GeometryError::UnsupportedSectorSize(bytes))
        }
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// `lba * bytes`，溢出返回 `None`。
    pub fn offset_of(self, lba: u64) -> Option<u64> {
        lba.checked_mul(self.0)
    }

    /// 字节偏移所在的 LBA；起点未对齐时返回 `None`。
    pub fn lba_of(self, offset: u64) -> Option<u64> {
        if offset % self.0 == 0 {
            Some(offset / self.0)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_allowed_set() {
        for bytes in SectorSize::ALLOWED {
            assert_eq!(SectorSize::new(bytes).map(SectorSize::bytes), Ok(bytes));
        }
        assert_eq!(SectorSize::new(0), Err(GeometryError::ZeroSectorSize));
        for bytes in [1, 256, 511, 513, 1000, 131_072] {
            assert_eq!(
                SectorSize::new(bytes),
                Err(GeometryError::UnsupportedSectorSize(bytes))
            );
        }
    }

    #[test]
    fn lba_conversions_round_trip() {
        let sector = SectorSize::new(4096).expect("4096 应被接受");
        assert_eq!(sector.offset_of(0), Some(0));
        assert_eq!(sector.offset_of(1), Some(4096));
        assert_eq!(sector.offset_of(u64::MAX), None);
        assert_eq!(sector.lba_of(0), Some(0));
        assert_eq!(sector.lba_of(4096), Some(1));
        assert_eq!(sector.lba_of(4095), None);
    }

    #[test]
    fn largest_sector_has_no_overflow_at_zero() {
        let sector = SectorSize::new(65536).expect("65536 应被接受");
        assert_eq!(sector.offset_of(0), Some(0));
        assert_eq!(
            sector.offset_of(u64::MAX / 65536),
            Some((u64::MAX / 65536) * 65536)
        );
        assert_eq!(sector.offset_of(u64::MAX / 65536 + 1), None);
    }
}
