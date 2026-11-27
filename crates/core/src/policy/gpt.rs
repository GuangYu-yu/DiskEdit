//! GPT 的元数据保留区与类型 GUID 约束。
//!
//! 这些函数只读已经解析出的表值，不读盘、不写盘。分区的类型 GUID 属于用户数据：
//! 几何变化可以移动分区、改动起止，但不得改写已有分区的类型 GUID。

use std::fmt;

use crate::region::{Region, RegionSet};
use crate::table::create::PmbrSize;
use crate::table::gpt::parse::{GptState, HeaderIssue, entry_array_span};
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

/// 修复动作：判定该用哪种方式把损坏的表收敛成双副本自洽的规范形态。
///
/// 动作本身即状态，不用布尔旗标；需要重算的 `last_usable_lba` 在判定期算好后随
/// 动作携带，写路径不再重新推导。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairAction {
    /// 无需修复。
    None,
    /// 备份头不在容器末端：重写双头，把备份副本搬到设备最后一个 LBA。
    RelocateBackup {
        new_backup_lba: u64,
        new_last_usable: u64,
    },
    /// 备份副本自洽，仅保护 MBR 的 SizeInLBA 不是本容器的口径。
    RepairProtectiveMbr,
    /// 两者都需处理。
    RelocateAndRepair {
        new_backup_lba: u64,
        new_last_usable: u64,
    },
}

impl RepairAction {
    /// 搬迁后的 `last_usable_lba`；不含搬迁的动作无值。
    pub fn new_last_usable(self) -> Option<u64> {
        match self {
            Self::RelocateBackup {
                new_last_usable, ..
            }
            | Self::RelocateAndRepair {
                new_last_usable, ..
            } => Some(new_last_usable),
            Self::None | Self::RepairProtectiveMbr => None,
        }
    }

    /// 是否要重写保护 MBR。
    pub fn rewrites_protective_mbr(self) -> bool {
        matches!(
            self,
            Self::RepairProtectiveMbr | Self::RelocateAndRepair { .. }
        )
    }

    /// 人类可读描述；`None` 动作没有描述。
    pub fn describe(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::RelocateBackup {
                new_backup_lba,
                new_last_usable,
            } => Some(format!(
                "relocate backup GPT → LBA {new_backup_lba} (last_usable_lba → {new_last_usable})"
            )),
            Self::RepairProtectiveMbr => Some(
                "rewrite protective MBR (SizeInLBA is not the logical-block value for this container)"
                    .to_owned(),
            ),
            Self::RelocateAndRepair {
                new_backup_lba,
                new_last_usable,
            } => Some(format!(
                "relocate backup GPT → LBA {new_backup_lba} (last_usable_lba → {new_last_usable}) \
                 + rewrite protective MBR (SizeInLBA is not the logical-block value for this container)"
            )),
        }
    }
}

/// 修复判定不能进行的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairError {
    /// 保护 MBR 的 SizeInLBA 大于两种口径：可能是更大盘的截断副本。
    ProtectiveMbrInconsistent,
    /// 容器容不下备份数组与备份头。
    ContainerTooSmall,
    /// 既有分区越出修复后的可用区上界。
    PartitionsBeyondNewUsable { max_end: u64, new_last_usable: u64 },
}

impl fmt::Display for RepairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtectiveMbrInconsistent => f.write_str(
                "protective MBR SizeInLBA exceeds the container — refusing auto-repair \
                 (use sgdisk/parted)",
            ),
            Self::ContainerTooSmall => f.write_str(
                "the container is too small for the GPT backup metadata — refusing auto-repair",
            ),
            Self::PartitionsBeyondNewUsable {
                max_end,
                new_last_usable,
            } => write!(
                f,
                "partitions end at LBA {max_end}, beyond the repaired last usable LBA \
                 {new_last_usable} — refusing to shrink the table under them"
            ),
        }
    }
}

/// 判定需要哪种修复，不写盘。两条轴：备份副本是否需要搬到容器末端（`state`）、
/// 保护 MBR 是否需要按本容器口径规范化（`pmbr`）。
///
/// 只有 `PmbrSize::Inconsistent` 拒绝自动修复：值大于两种口径，可能是更大盘的
/// 截断副本，此时"按本容器重写"会抹掉线人线索。主副本不可读时可用区上界取
/// `min(备份头的 last_usable, 容器末端减跨度)`，不让重定位静默抬高它；备份缺失
/// 或两副本分歧时以主头为权威，保持它的 `last_usable` 不变。
pub fn repair_action(
    header: &GptHeader,
    entries: &[PartitionEntry],
    state: GptState,
    pmbr: PmbrSize,
    container_last_lba: u64,
    sector_size: u64,
) -> Result<RepairAction, RepairError> {
    if pmbr == PmbrSize::Inconsistent {
        return Err(RepairError::ProtectiveMbrInconsistent);
    }
    let rewrites_pmbr = matches!(pmbr, PmbrSize::NeedsRepair { .. });
    if state == GptState::Valid {
        return Ok(if rewrites_pmbr {
            RepairAction::RepairProtectiveMbr
        } else {
            RepairAction::None
        });
    }
    let span = entry_array_span(
        header.number_of_partition_entries,
        header.size_of_partition_entry,
        sector_size,
    )
    .map_err(|_| RepairError::ContainerTooSmall)?;
    let computed = container_last_lba
        .checked_sub(span)
        .and_then(|value| value.checked_sub(1))
        .ok_or(RepairError::ContainerTooSmall)?;
    let (new_last_usable, bounds_from_container) = match state {
        GptState::NeedsRepair {
            cause: HeaderIssue::PrimaryUnreadable,
        } => (header.last_usable_lba.min(computed), true),
        GptState::NeedsRepair {
            cause: HeaderIssue::BackupUnreadable | HeaderIssue::CopiesDiverge,
        } => (header.last_usable_lba, false),
        GptState::NeedsRepair {
            cause: HeaderIssue::BackupLbaStale { .. },
        } => (computed, true),
        GptState::Valid => (computed, true),
    };
    if bounds_from_container {
        let max_end = entries
            .iter()
            .filter(|entry| !entry.partition_type_guid.is_zero())
            .map(|entry| entry.ending_lba)
            .max()
            .unwrap_or(0);
        if max_end > new_last_usable {
            return Err(RepairError::PartitionsBeyondNewUsable {
                max_end,
                new_last_usable,
            });
        }
    }
    Ok(if rewrites_pmbr {
        RepairAction::RelocateAndRepair {
            new_backup_lba: container_last_lba,
            new_last_usable,
        }
    } else {
        RepairAction::RelocateBackup {
            new_backup_lba: container_last_lba,
            new_last_usable,
        }
    })
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

    fn stale_backup() -> GptState {
        GptState::NeedsRepair {
            cause: HeaderIssue::BackupLbaStale {
                expected: 1000,
                actual: 500,
            },
        }
    }

    fn pmbr_stale() -> PmbrSize {
        PmbrSize::NeedsRepair {
            cause: crate::table::create::PmbrIssue::Stale,
        }
    }

    /// 判定矩阵：两条轴四组合 + Inconsistent 一票否决，与旧版语义逐格对应。
    #[test]
    fn repair_action_decision_matrix() {
        let normal = PmbrSize::Normal;
        let valid = GptState::Valid;
        let base = header();
        assert_eq!(
            repair_action(&base, &[], valid, normal, 2000, 512),
            Ok(RepairAction::None)
        );
        assert_eq!(
            repair_action(&base, &[], valid, pmbr_stale(), 2000, 512),
            Ok(RepairAction::RepairProtectiveMbr)
        );
        assert_eq!(
            repair_action(&base, &[], stale_backup(), normal, 2000, 512),
            Ok(RepairAction::RelocateBackup {
                new_backup_lba: 2000,
                new_last_usable: 2000 - 32 - 1,
            })
        );
        assert_eq!(
            repair_action(&base, &[], stale_backup(), pmbr_stale(), 2000, 512),
            Ok(RepairAction::RelocateAndRepair {
                new_backup_lba: 2000,
                new_last_usable: 2000 - 32 - 1,
            })
        );
        assert_eq!(
            repair_action(&base, &[], valid, PmbrSize::Inconsistent, 2000, 512),
            Err(RepairError::ProtectiveMbrInconsistent)
        );
    }

    /// 主副本不可读时，可用区上界取备份头值与容器计算值的较小者。
    #[test]
    fn primary_unreadable_keeps_the_lower_last_usable() {
        let state = GptState::NeedsRepair {
            cause: HeaderIssue::PrimaryUnreadable,
        };
        let action = repair_action(&header(), &[], state, PmbrSize::Normal, 2000, 512)
            .expect("主头丢失可修复");
        assert_eq!(
            action,
            RepairAction::RelocateBackup {
                new_backup_lba: 2000,
                new_last_usable: 967,
            }
        );
        assert_eq!(action.new_last_usable(), Some(967));
        assert!(!action.rewrites_protective_mbr());
    }

    /// 备份缺失或两副本分歧时以主头为权威，不改写可用区上界。
    #[test]
    fn backup_side_faults_keep_the_primary_last_usable() {
        for cause in [HeaderIssue::BackupUnreadable, HeaderIssue::CopiesDiverge] {
            let state = GptState::NeedsRepair { cause };
            let action = repair_action(&header(), &[], state, PmbrSize::Normal, 2000, 512)
                .expect("备份侧损坏可修复");
            assert_eq!(
                action,
                RepairAction::RelocateBackup {
                    new_backup_lba: 2000,
                    new_last_usable: 967,
                }
            );
        }
    }

    /// 容器容不下备份元数据，或既有分区越出新上界，都必须拒绝而不是截断分区。
    #[test]
    fn repair_action_refuses_impossible_geometry() {
        assert_eq!(
            repair_action(&header(), &[], stale_backup(), PmbrSize::Normal, 32, 512),
            Err(RepairError::ContainerTooSmall)
        );
        let mut far = entry(1, 0x0c);
        far.ending_lba = 1990;
        assert!(matches!(
            repair_action(
                &header(),
                &[far],
                stale_backup(),
                PmbrSize::Normal,
                2000,
                512
            ),
            Err(RepairError::PartitionsBeyondNewUsable { .. })
        ));
    }

    /// `Inconsistent` 在无需搬迁时也必须拒绝，而不是按普通状态放行。
    #[test]
    fn inconsistent_protective_mbr_is_refused_even_with_a_valid_table() {
        assert_eq!(
            repair_action(
                &header(),
                &[],
                GptState::Valid,
                PmbrSize::Inconsistent,
                2000,
                512
            ),
            Err(RepairError::ProtectiveMbrInconsistent)
        );
    }

    #[test]
    fn describe_covers_every_action_but_none() {
        assert_eq!(RepairAction::None.describe(), None);
        assert!(RepairAction::RepairProtectiveMbr.describe().is_some());
        let relocate = RepairAction::RelocateBackup {
            new_backup_lba: 10,
            new_last_usable: 5,
        };
        assert!(
            relocate
                .describe()
                .is_some_and(|text| text.contains("LBA 10"))
        );
    }
}
