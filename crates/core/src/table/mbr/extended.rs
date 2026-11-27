//! 扩展分区链：从首 EBR 出发逐级读取逻辑分区。
//!
//! 每一级 EBR 的第一条记录指向下一级 EBR（相对当前 EBR），第二条记录是本级逻辑分区
//! （相对当前 EBR 的起点）。读取由调用方提供的闭包完成，因此本模块仍是纯函数。

use super::super::TableError;
use super::primary::{MBR_ENTRY_BASE, MBR_ENTRY_BYTES, MBR_SIGNATURE_OFFSET, MbrEntry};

/// 扩展链允许的最大逻辑分区数。
pub const MAX_LOGICAL: usize = 128;

/// 遍历扩展链，返回逻辑分区（起始 LBA 已换算为绝对地址）。
///
/// `read` 必须按绝对 LBA 返回该扇区字节；返回 `Err` 时整个遍历失败。
pub fn walk_extended(
    first_ebr_lba: u64,
    mut read: impl FnMut(u64) -> Result<Vec<u8>, TableError>,
) -> Result<Vec<MbrEntry>, TableError> {
    let mut logical = Vec::new();
    let mut current = first_ebr_lba;
    let mut visited = Vec::new();
    while current != 0 {
        if visited.contains(&current) {
            return Err(TableError::ExtendedChainLoop { lba: current });
        }
        visited.push(current);
        if logical.len() >= MAX_LOGICAL {
            return Err(TableError::ExtendedChainTooLong { max: MAX_LOGICAL });
        }
        let sector = read(current)?;
        if sector.len() < MBR_ENTRY_BASE + 2 * MBR_ENTRY_BYTES {
            return Err(TableError::BufferTooSmall {
                needed: MBR_ENTRY_BASE + 2 * MBR_ENTRY_BYTES,
                actual: sector.len(),
            });
        }
        // 每一级 EBR 先验 0x55AA 再推进：链中段坏签名的扇区不是 EBR，按签名缺失
        // 拒绝，不把它当空表继续走（否则会把后续垃圾扇区误读成逻辑分区）。
        if sector[MBR_SIGNATURE_OFFSET] != 0x55 || sector[MBR_SIGNATURE_OFFSET + 1] != 0xAA {
            return Err(TableError::MbrSignatureMissing);
        }
        let link = parse_entry(&sector[MBR_ENTRY_BASE..MBR_ENTRY_BASE + MBR_ENTRY_BYTES]);
        let data = parse_entry(
            &sector[MBR_ENTRY_BASE + MBR_ENTRY_BYTES..MBR_ENTRY_BASE + 2 * MBR_ENTRY_BYTES],
        );
        if !data.is_empty() {
            let start = current.checked_add(u64::from(data.start_lba)).ok_or(
                TableError::InvalidGeometry("logical partition start overflow"),
            )?;
            logical.push(MbrEntry {
                boot: data.boot,
                part_type: data.part_type,
                start_lba: u32::try_from(start).map_err(|_| {
                    TableError::InvalidGeometry("logical partition start exceeds 32 bits")
                })?,
                size_lba: data.size_lba,
            });
        }
        if link.is_extended() {
            current = current
                .checked_add(u64::from(link.start_lba))
                .ok_or(TableError::InvalidGeometry("next EBR overflow"))?;
        } else {
            current = 0;
        }
    }
    Ok(logical)
}

fn parse_entry(raw: &[u8]) -> MbrEntry {
    MbrEntry {
        boot: raw[0],
        part_type: raw[4],
        start_lba: u32::from_le_bytes(raw[8..12].try_into().expect("偏移已校验")),
        size_lba: u32::from_le_bytes(raw[12..16].try_into().expect("偏移已校验")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::mbr::primary::MBR_SIGNATURE_OFFSET;

    fn ebr(link: Option<(u8, u32, u32)>, data: Option<(u8, u32, u32)>) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        let write = |sector: &mut Vec<u8>, slot: usize, entry: (u8, u32, u32)| {
            let offset = MBR_ENTRY_BASE + slot * MBR_ENTRY_BYTES;
            sector[offset + 4] = entry.0;
            sector[offset + 8..offset + 12].copy_from_slice(&entry.1.to_le_bytes());
            sector[offset + 12..offset + 16].copy_from_slice(&entry.2.to_le_bytes());
        };
        if let Some(entry) = link {
            write(&mut sector, 0, entry);
        }
        if let Some(entry) = data {
            write(&mut sector, 1, entry);
        }
        sector[MBR_SIGNATURE_OFFSET] = 0x55;
        sector[MBR_SIGNATURE_OFFSET + 1] = 0xAA;
        sector
    }

    #[test]
    fn walks_a_single_logical_partition() {
        let image = ebr(None, Some((0x83, 63, 100)));
        let read = |lba| {
            assert_eq!(lba, 2048);
            Ok(image.clone())
        };
        let entries = walk_extended(2048, read).expect("应可遍历");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].start_lba, 2048 + 63);
    }

    #[test]
    fn stops_when_link_is_absent() {
        let image = ebr(None, Some((0x83, 1, 10)));
        let entries = walk_extended(100, |_| Ok(image.clone())).expect("应可遍历");
        assert_eq!(entries.len(), 1);
    }

    // covers: D17
    #[test]
    fn rejects_a_mid_chain_sector_without_the_boot_signature() {
        // 首 EBR 正常，链接指向的下一级 EBR 丢了 0x55AA：整条链按签名缺失拒绝。
        let first = ebr(Some((0x0F, 50, 0)), Some((0x83, 1, 10)));
        let mut second = ebr(None, Some((0x83, 1, 10)));
        second[MBR_SIGNATURE_OFFSET] = 0;
        let result = walk_extended(100, |lba| {
            if lba == 100 {
                Ok(first.clone())
            } else {
                Ok(second.clone())
            }
        });
        assert_eq!(result, Err(TableError::MbrSignatureMissing));
    }

    #[test]
    fn detects_loops() {
        let image = ebr(Some((0x0F, 0, 0)), None);
        let result = walk_extended(50, |_| Ok(image.clone()));
        assert_eq!(result, Err(TableError::ExtendedChainLoop { lba: 50 }));
    }

    #[test]
    fn rejects_overlong_chain() {
        let image = ebr(Some((0x0F, 1, 0)), Some((0x83, 0, 8)));
        let result = walk_extended(100, |lba| {
            let _ = lba;
            Ok(image.clone())
        });
        assert_eq!(
            result,
            Err(TableError::ExtendedChainTooLong { max: MAX_LOGICAL })
        );
    }
}
