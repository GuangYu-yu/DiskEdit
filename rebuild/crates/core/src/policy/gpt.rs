//! GPT 的元数据保留区与类型 GUID 约束。
//!
//! 这些函数只读已经解析出的表值，不读盘、不写盘。分区的类型 GUID 属于用户数据：
//! 几何变化可以移动分区、改动起止，但不得改写已有分区的类型 GUID。

use crate::region::{Region, RegionSet};
use crate::table::gpt::parse::entry_array_span;
use crate::table::{GptHeader, Guid, PRIMARY_HEADER_LBA, PartitionEntry, TableError};

/// 一个已有分区的类型 GUID 被改写。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeGuidChange {
    pub unique_partition_guid: Guid,
    pub from: Guid,
    pub to: Guid,
}

/// GPT 元数据占用的 LBA 区间：保护性 MBR、主头、主项数组、备份项数组、备份头。
///
/// 结果为半开区间集合，单位是 LBA。备份项数组位于容器末端、备份头之前，
/// 其起点为 `last_lba - span`；容器容不下时返回错误而不是截断。
pub fn metadata_regions(
    header: &GptHeader,
    last_lba: u64,
    sector_size: u64,
) -> Result<RegionSet, TableError> {
    let span = entry_array_span(
        header.number_of_partition_entries,
        header.size_of_partition_entry,
        sector_size,
    )?;
    let primary_end =
        header
            .partition_entry_lba
            .checked_add(span)
            .ok_or(TableError::InvalidGeometry(
                "primary entry array range overflow",
            ))?;
    let backup_start = last_lba
        .checked_sub(span)
        .ok_or(TableError::InvalidGeometry(
            "backup entry array underflows the container",
        ))?;
    let backup_header_end = last_lba
        .checked_add(1)
        .ok_or(TableError::InvalidGeometry("backup header range overflow"))?;

    let mut set = RegionSet::new();
    set.insert(lba_region(0, 1)?);
    set.insert(lba_region(PRIMARY_HEADER_LBA, 1)?);
    set.insert(Region::new(header.partition_entry_lba, primary_end).map_err(region_error)?);
    set.insert(Region::new(backup_start, last_lba).map_err(region_error)?);
    set.insert(Region::new(last_lba, backup_header_end).map_err(region_error)?);
    Ok(set)
}

/// 可供分区使用的 LBA 区间 `[first_usable_lba, last_usable_lba]`，转为半开区间。
///
/// `last_usable_lba` 是闭区间末端，因此结果终点为 `last_usable_lba + 1`；
/// 越过 `u64` 上界时返回错误而不是回绕。
pub fn usable_range(header: &GptHeader) -> Result<Region, TableError> {
    if header.first_usable_lba > header.last_usable_lba {
        return Err(TableError::InvalidGeometry(
            "first_usable_lba exceeds last_usable_lba",
        ));
    }
    let end = header
        .last_usable_lba
        .checked_add(1)
        .ok_or(TableError::InvalidGeometry("usable range overflow"))?;
    Region::new(header.first_usable_lba, end).map_err(region_error)
}

/// 校验两张表之间，仍然存在的分区没有改写类型 GUID。
///
/// 以 `unique_partition_guid` 认定"同一个分区"；零 GUID 不参与比对，
/// 因为它不构成可追踪的身份。被删除的分区不在本约束内。
pub fn preserved_type_guids(
    before: &[PartitionEntry],
    after: &[PartitionEntry],
) -> Result<(), TypeGuidChange> {
    for old in before {
        if old.unique_partition_guid.is_zero() {
            continue;
        }
        let Some(new) = after
            .iter()
            .find(|entry| entry.unique_partition_guid == old.unique_partition_guid)
        else {
            continue;
        };
        if new.partition_type_guid != old.partition_type_guid {
            return Err(TypeGuidChange {
                unique_partition_guid: old.unique_partition_guid,
                from: old.partition_type_guid,
                to: new.partition_type_guid,
            });
        }
    }
    Ok(())
}

fn lba_region(start: u64, len: u64) -> Result<Region, TableError> {
    Region::sized(start, len).map_err(region_error)
}

fn region_error(error: crate::region::RegionError) -> TableError {
    match error {
        crate::region::RegionError::Inverted { start, end } => {
            TableError::InvalidGeometry(if start > end {
                "region start exceeds end"
            } else {
                "region length overflows the address space"
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::gpt::entries::PartitionName;
    use crate::table::{ENTRY_MIN_BYTES, MAX_ENTRIES};

    fn header() -> GptHeader {
        GptHeader {
            revision: 0x0001_0000,
            primary_lba: PRIMARY_HEADER_LBA,
            backup_lba: 1000,
            first_usable_lba: 34,
            last_usable_lba: 967,
            disk_guid: Guid::ZERO,
            partition_entry_lba: 2,
            number_of_partition_entries: MAX_ENTRIES,
            size_of_partition_entry: ENTRY_MIN_BYTES as u32,
            header_size: 92,
            entry_array_crc32: 0,
        }
    }

    fn entry(unique: u8, type_guid: u8) -> PartitionEntry {
        let mut unique_bytes = [0u8; 16];
        unique_bytes[0] = unique;
        let mut type_bytes = [0u8; 16];
        type_bytes[0] = type_guid;
        PartitionEntry {
            partition_type_guid: Guid(type_bytes),
            unique_partition_guid: Guid(unique_bytes),
            starting_lba: 34,
            ending_lba: 40,
            attributes: 0,
            name: PartitionName::EMPTY,
        }
    }

    #[test]
    fn metadata_regions_cover_mbr_headers_and_both_arrays() {
        let set = metadata_regions(&header(), 1000, 512).expect("几何应可计算");
        // 主项数组 128 × 128 B = 16384 B = 32 LBA，位于 [2, 34)。
        // 备份项数组位于 [1000 - 32, 1000) = [968, 1000)，备份头为 [1000, 1001)。
        let regions: Vec<(u64, u64)> = set.iter().map(|r| (r.start(), r.end())).collect();
        assert_eq!(regions, vec![(0, 34), (968, 1001)]);
        assert!(set.contains_offset(0));
        assert!(set.contains_offset(1));
        assert!(set.contains_offset(33));
        assert!(set.contains_offset(968));
        assert!(set.contains_offset(1000));
        assert!(!set.contains_offset(34));
        assert!(!set.contains_offset(967));
    }

    #[test]
    fn metadata_regions_scale_with_sector_size() {
        // 4096 字节扇区时同一数组只占 4 LBA：主 [2, 6)，备份 [996, 1000)。
        let set = metadata_regions(&header(), 1000, 4096).expect("几何应可计算");
        let regions: Vec<(u64, u64)> = set.iter().map(|r| (r.start(), r.end())).collect();
        assert_eq!(regions, vec![(0, 6), (996, 1001)]);
    }

    #[test]
    fn metadata_regions_reject_container_too_small_for_backup_array() {
        let error = metadata_regions(&header(), 10, 512).expect_err("容器放不下备份项数组");
        assert!(matches!(error, TableError::InvalidGeometry(_)));
    }

    #[test]
    fn usable_range_is_half_open_and_inclusive_at_the_end() {
        let range = usable_range(&header()).expect("可用区间应成立");
        assert_eq!((range.start(), range.end()), (34, 968));
    }

    #[test]
    fn usable_range_rejects_inverted_bounds() {
        let mut bad = header();
        bad.first_usable_lba = 968;
        bad.last_usable_lba = 967;
        assert!(usable_range(&bad).is_err());
    }

    #[test]
    fn preserved_type_guids_allows_new_and_deleted_partitions() {
        let before = vec![entry(1, 0x0c)];
        let after = vec![entry(1, 0x0c), entry(2, 0x07)];
        assert_eq!(preserved_type_guids(&before, &after), Ok(()));

        let deleted = vec![entry(2, 0x07)];
        assert_eq!(preserved_type_guids(&before, &deleted), Ok(()));
    }

    #[test]
    fn preserved_type_guids_rejects_a_rewritten_type() {
        let before = vec![entry(1, 0x0c)];
        let after = vec![entry(1, 0x07)];
        let change = preserved_type_guids(&before, &after).expect_err("类型 GUID 被改写");
        assert_eq!(change.unique_partition_guid.0[0], 1);
        assert_eq!(change.from.0[0], 0x0c);
        assert_eq!(change.to.0[0], 0x07);
    }

    #[test]
    fn preserved_type_guids_ignores_entries_without_identity() {
        let mut before = entry(1, 0x0c);
        before.unique_partition_guid = Guid::ZERO;
        let mut after = entry(1, 0x07);
        after.unique_partition_guid = Guid::ZERO;
        assert_eq!(preserved_type_guids(&[before], &[after]), Ok(()));
    }
}
