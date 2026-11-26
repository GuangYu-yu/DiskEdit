//! 单位换算：二进制尺寸常量与扇区取整。
//!
//! 所有换算都以 `u64` 字节为基准，溢出返回 `None`。
//! 十进制后缀的解析属于命令行层，不在纯核内。

use super::GeometryError;
use super::sector::SectorSize;

pub const KIB: u64 = 1024;
pub const MIB: u64 = KIB * 1024;
pub const GIB: u64 = MIB * 1024;
pub const TIB: u64 = GIB * 1024;
pub const PIB: u64 = TIB * 1024;

/// 覆盖 `bytes` 所需的最少扇区数，向上取整。
pub fn sectors_ceil(bytes: u64, sector: SectorSize) -> Option<u64> {
    let size = sector.bytes();
    let full = bytes / size;
    if bytes.is_multiple_of(size) {
        Some(full)
    } else {
        full.checked_add(1)
    }
}

/// 扇区数换算为字节数。
pub fn bytes_of_sectors(sectors: u64, sector: SectorSize) -> Option<u64> {
    sectors.checked_mul(sector.bytes())
}

/// 按扇区向下取整的字节数。
pub fn floor_to_sector(bytes: u64, sector: SectorSize) -> u64 {
    bytes - bytes % sector.bytes()
}

/// 按扇区向上取整的字节数。
pub fn ceil_to_sector(bytes: u64, sector: SectorSize) -> Result<u64, GeometryError> {
    let sectors = sectors_ceil(bytes, sector).ok_or(GeometryError::Overflow)?;
    bytes_of_sectors(sectors, sector).ok_or(GeometryError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sector(bytes: u64) -> SectorSize {
        SectorSize::new(bytes).expect("测试扇区应合法")
    }

    #[test]
    fn binary_constants_are_consistent() {
        assert_eq!(MIB / KIB, 1024);
        assert_eq!(GIB / MIB, 1024);
        assert_eq!(TIB / GIB, 1024);
        assert_eq!(PIB / TIB, 1024);
    }

    #[test]
    fn sector_counts_round_up() {
        let s = sector(512);
        assert_eq!(sectors_ceil(0, s), Some(0));
        assert_eq!(sectors_ceil(1, s), Some(1));
        assert_eq!(sectors_ceil(512, s), Some(1));
        assert_eq!(sectors_ceil(513, s), Some(2));
        assert_eq!(sectors_ceil(1023, s), Some(2));
    }

    #[test]
    fn byte_conversions_cover_extremes() {
        let s = sector(4096);
        assert_eq!(bytes_of_sectors(0, s), Some(0));
        assert_eq!(bytes_of_sectors(1, s), Some(4096));
        assert_eq!(bytes_of_sectors(u64::MAX, s), None);
        assert_eq!(ceil_to_sector(0, s), Ok(0));
        assert_eq!(ceil_to_sector(1, s), Ok(4096));
        assert_eq!(ceil_to_sector(u64::MAX, s), Err(GeometryError::Overflow));
    }

    #[test]
    fn floor_handles_unaligned() {
        let s = sector(512);
        assert_eq!(floor_to_sector(0, s), 0);
        assert_eq!(floor_to_sector(511, s), 0);
        assert_eq!(floor_to_sector(512, s), 512);
        assert_eq!(floor_to_sector(1025, s), 1024);
    }

    #[test]
    fn cross_unit_sizes_stay_exact() {
        let s = sector(4096);
        assert_eq!(bytes_of_sectors(MIB / 4096, s), Some(MIB));
        assert_eq!(sectors_ceil(GIB, s), Some(GIB / 4096));
    }
}
