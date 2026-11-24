//! 条目级编辑：纯值计算，不读盘。
//!
//! 这里只改变 `PartitionEntry` 与 `MbrEntry` 的值；写回扇区与落盘由 `table`
//! 模块负责。

use diskedit_core::table::gpt::entries::is_used;
use diskedit_core::table::{Guid, Mbr, MbrEntry, PartitionEntry, PartitionName};

/// 空 GPT 条目。
pub const EMPTY_GPT: PartitionEntry = PartitionEntry {
    partition_type_guid: Guid::ZERO,
    unique_partition_guid: Guid::ZERO,
    starting_lba: 0,
    ending_lba: 0,
    attributes: 0,
    name: PartitionName::EMPTY,
};

/// 由计划摘要派生一个非零 GUID；同一计划与同一标签得到同一值。
pub fn guid_from_seed(seed: &[u8; 32], tag: u64) -> Guid {
    let mut bytes = [0u8; 16];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let source = seed[index % seed.len()];
        let mixed = seed[(index + 7) % seed.len()].rotate_left((tag % 8) as u32);
        *slot = source ^ mixed ^ (tag as u8);
    }
    // 版本 4 与变体位，避免产生保留形态。
    bytes[7] = (bytes[7] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Guid(bytes)
}

/// 找到第一个空条目下标。
pub fn free_slot(entries: &[PartitionEntry]) -> Option<usize> {
    entries.iter().position(|entry| !is_used(entry))
}

/// 按 1 起的分区号取条目下标。
pub fn index_of(partition: u32) -> Result<usize, String> {
    let index = usize::try_from(partition)
        .ok()
        .and_then(|number| number.checked_sub(1))
        .ok_or_else(|| format!("invalid partition number {partition}"))?;
    Ok(index)
}

/// 在空槽写入一个新条目。
pub fn add_gpt(
    entries: &mut [PartitionEntry],
    start_lba: u64,
    sectors: u64,
    type_guid: Guid,
    name: &str,
    unique: Guid,
    part_number: Option<u32>,
) -> Result<u32, String> {
    if sectors == 0 {
        return Err("new partition length is zero".to_owned());
    }
    let index = match part_number {
        Some(number) => {
            let index = index_of(number)?;
            let slot = entries
                .get(index)
                .ok_or_else(|| format!("partition number {number} beyond entry array"))?;
            if is_used(slot) {
                return Err(format!("partition number {number} is in use"));
            }
            index
        }
        None => free_slot(entries).ok_or_else(|| "no free partition entry".to_owned())?,
    };
    entries[index] = PartitionEntry {
        partition_type_guid: type_guid,
        unique_partition_guid: unique,
        starting_lba: start_lba,
        ending_lba: start_lba + sectors - 1,
        attributes: 0,
        name: PartitionName::from_str_lossy(name),
    };
    Ok(u32::try_from(index + 1).expect("entry index fits in u32"))
}

/// 清零一个条目。
pub fn delete_gpt(entries: &mut [PartitionEntry], partition: u32) -> Result<(), String> {
    let index = index_of(partition)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond entry array"))?;
    if !is_used(entry) {
        return Err(format!("partition {partition} not found"));
    }
    *entry = EMPTY_GPT;
    Ok(())
}

/// 改变一个条目的起止。
pub fn resize_gpt(
    entries: &mut [PartitionEntry],
    partition: u32,
    start_lba: u64,
    sectors: u64,
) -> Result<(), String> {
    if sectors == 0 {
        return Err("partition length cannot be zero".to_owned());
    }
    let index = index_of(partition)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond entry array"))?;
    if !is_used(entry) {
        return Err(format!("partition {partition} not found"));
    }
    entry.starting_lba = start_lba;
    entry.ending_lba = start_lba + sectors - 1;
    Ok(())
}

/// 修改分区名。
pub fn set_name(entries: &mut [PartitionEntry], partition: u32, value: &str) -> Result<(), String> {
    let index = index_of(partition)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond entry array"))?;
    if !is_used(entry) {
        return Err(format!("partition {partition} not found"));
    }
    entry.name = PartitionName::from_str_lossy(value);
    Ok(())
}

/// 修改 GPT 属性字。
pub fn set_attributes(
    entries: &mut [PartitionEntry],
    partition: u32,
    value: u64,
) -> Result<(), String> {
    let index = index_of(partition)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond entry array"))?;
    if !is_used(entry) {
        return Err(format!("partition {partition} not found"));
    }
    entry.attributes = value;
    Ok(())
}

/// 置位或清除 GPT 属性字的第 `flag` 位。
pub fn set_attribute_bit(
    entries: &mut [PartitionEntry],
    partition: u32,
    flag: u32,
    on: bool,
) -> Result<(), String> {
    if flag >= 64 {
        return Err(format!(
            "attribute bit {flag} beyond the 64-bit attribute word"
        ));
    }
    let index = index_of(partition)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond entry array"))?;
    if !is_used(entry) {
        return Err(format!("partition {partition} not found"));
    }
    let mask = 1u64 << flag;
    entry.attributes = if on {
        entry.attributes | mask
    } else {
        entry.attributes & !mask
    };
    Ok(())
}

/// 修改分区类型 GUID。
pub fn set_type(entries: &mut [PartitionEntry], partition: u32, guid: Guid) -> Result<(), String> {
    if guid.is_zero() {
        return Err("partition type cannot be empty".to_owned());
    }
    let index = index_of(partition)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond entry array"))?;
    if !is_used(entry) {
        return Err(format!("partition {partition} not found"));
    }
    entry.partition_type_guid = guid;
    Ok(())
}

/// 在 MBR 主表写一个条目。
pub fn add_mbr(
    mbr: &mut Mbr,
    start_lba: u64,
    sectors: u64,
    part_type: u8,
    part_number: Option<u32>,
) -> Result<u32, String> {
    if sectors == 0 {
        return Err("new partition length is zero".to_owned());
    }
    let start = u32::try_from(start_lba).map_err(|_| "start LBA beyond MBR range".to_owned())?;
    let size =
        u32::try_from(sectors).map_err(|_| "partition length beyond MBR range".to_owned())?;
    let index = match part_number {
        Some(number) => {
            let index = index_of(number)?;
            let slot = mbr.entries.get(index).ok_or_else(|| {
                format!("partition number {number} beyond primary partition slots")
            })?;
            if !slot.is_empty() {
                return Err(format!("partition number {number} is in use"));
            }
            index
        }
        None => mbr
            .entries
            .iter()
            .position(|entry| entry.is_empty())
            .ok_or_else(|| "no free primary partition slot".to_owned())?,
    };
    mbr.entries[index] = MbrEntry {
        boot: 0,
        part_type,
        start_lba: start,
        size_lba: size,
    };
    Ok(u32::try_from(index + 1).expect("primary slot fits in u32"))
}

/// 清空 MBR 的一个主分区。
pub fn delete_mbr(mbr: &mut Mbr, partition: u32) -> Result<(), String> {
    let index = index_of(partition)?;
    let entry = mbr
        .entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond primary partition slots"))?;
    if entry.is_empty() {
        return Err(format!("partition {partition} not found"));
    }
    *entry = MbrEntry::default();
    Ok(())
}

/// 改变 MBR 主分区的起止。
pub fn resize_mbr(
    mbr: &mut Mbr,
    partition: u32,
    start_lba: u64,
    sectors: u64,
) -> Result<(), String> {
    if sectors == 0 {
        return Err("partition length cannot be zero".to_owned());
    }
    let index = index_of(partition)?;
    let entry = mbr
        .entries
        .get_mut(index)
        .ok_or_else(|| format!("partition {partition} beyond primary partition slots"))?;
    if entry.is_empty() {
        return Err(format!("partition {partition} not found"));
    }
    entry.start_lba =
        u32::try_from(start_lba).map_err(|_| "start LBA beyond MBR range".to_owned())?;
    entry.size_lba =
        u32::try_from(sectors).map_err(|_| "partition length beyond MBR range".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<PartitionEntry> {
        vec![EMPTY_GPT; 4]
    }

    #[test]
    fn derived_guid_is_non_zero_and_stable() {
        let seed = [0u8; 32];
        let first = guid_from_seed(&seed, 1);
        assert!(!first.is_zero());
        assert_eq!(first, guid_from_seed(&seed, 1));
        assert_ne!(first, guid_from_seed(&seed, 2));
    }

    #[test]
    fn add_uses_first_free_slot() {
        let mut table = entries();
        let number = add_gpt(
            &mut table,
            2048,
            2048,
            Guid([1; 16]),
            "root",
            Guid([2; 16]),
            None,
        )
        .expect("新增应成立");
        assert_eq!(number, 1);
        assert_eq!(table[0].starting_lba, 2048);
        assert_eq!(table[0].ending_lba, 4095);
        assert_eq!(table[0].name.to_string_lossy(), "root");
    }

    #[test]
    fn delete_and_resize_reject_missing_partitions() {
        let mut table = entries();
        assert!(delete_gpt(&mut table, 1).is_err());
        assert!(resize_gpt(&mut table, 1, 0, 1).is_err());
    }

    #[test]
    fn mbr_add_delete_round_trip() {
        let mut mbr = Mbr {
            entries: [MbrEntry::default(); 4],
        };
        add_mbr(&mut mbr, 63, 100, 0x83, None).expect("新增应成立");
        assert_eq!(mbr.entries[0].start_lba, 63);
        delete_mbr(&mut mbr, 1).expect("删除应成立");
        assert!(mbr.entries[0].is_empty());
    }
}
