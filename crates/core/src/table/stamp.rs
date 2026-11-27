//! 表区摘要：把首/次/末三个扇区折叠成 32 字节，用于写前"字节未变"核对。
//!
//! 摘要不承诺密码学强度，只承诺同输入同输出、任一字节不同即不同。四条独立
//! FNV-1a 通道拼成 32 字节，避免单通道在短输入上的碰撞面。

use crate::region::Region;

/// 写前摘要取样的扇区：首、第二与末扇区，去重后按起点升序。
///
/// 观察层与执行层都必须用同一组取样点，否则"写前摘要"无法复核。
pub fn stamp_regions(total_bytes: u64, sector_size: u64) -> Vec<Region> {
    if total_bytes == 0 || sector_size == 0 {
        return Vec::new();
    }
    let last_start = (total_bytes / sector_size).saturating_sub(1) * sector_size;
    let mut regions = Vec::new();
    for offset in [0u64, sector_size, last_start] {
        if offset >= total_bytes {
            continue;
        }
        let len = sector_size.min(total_bytes - offset);
        if let Ok(region) = Region::sized(offset, len) {
            regions.push(region);
        }
    }
    regions.sort_by_key(|region| region.start());
    regions.dedup_by_key(|region| region.start());
    regions
}

/// 计算多段字节的摘要；段与段的边界参与折叠（不同切分得到不同结果）。
pub fn table_stamp(parts: &[&[u8]]) -> [u8; 32] {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = [0u8; 32];
    for lane in 0u64..4 {
        let mut hash = BASIS ^ lane.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for (index, part) in parts.iter().enumerate() {
            hash ^= index as u64;
            hash = hash.wrapping_mul(PRIME);
            for &byte in *part {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(PRIME);
            }
        }
        let at = lane as usize * 8;
        out[at..at + 8].copy_from_slice(&hash.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_agree_and_any_change_differs() {
        let a = table_stamp(&[b"abc", b"def"]);
        assert_eq!(a, table_stamp(&[b"abc", b"def"]));
        assert_ne!(a, table_stamp(&[b"abd", b"def"]));
        assert_ne!(a, table_stamp(&[b"abcdef", b""]));
    }

    #[test]
    fn empty_input_is_stable() {
        assert_eq!(table_stamp(&[]), table_stamp(&[]));
    }
}
