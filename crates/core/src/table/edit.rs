//! 表编辑：在已读出的表上做纯计算的新增、删除与属性修改。
//!
//! 本模块只回答"改动后的字节是什么"与"这次改动为什么不能落盘"；读盘由观察层
//! 完成，写盘由执行层的唯一写通道完成。GPT 的编辑结果是一组双副本写段，MBR 的
//! 编辑结果是 LBA0 的整段字节。

use std::fmt;

use crate::region::Region;
use crate::table::create::Segment;
use crate::table::gpt::entries::{PartitionEntry, PartitionName, serialize_entries};
use crate::table::gpt::header::GptHeader;
use crate::table::gpt::parse::entry_array_span;
use crate::table::gpt::write::array_crc;
use crate::table::mbr::primary::{
    MBR_ENTRY_BASE, MBR_ENTRY_BYTES, MBR_SIGNATURE_OFFSET, MbrEntry, TYPE_EXTENDED,
    TYPE_EXTENDED_LBA, TYPE_EXTENDED_LINUX, TYPE_PROTECTIVE,
};
use crate::table::{Guid, PRIMARY_ARRAY_LBA, PRIMARY_HEADER_LBA, TableError};

/// 新 GPT 条目的默认类型：Linux filesystem data。
pub const LINUX_FS_TYPE_GUID: Guid = Guid([
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
]);
/// EFI System Partition 的类型 GUID。
pub const ESP_TYPE_GUID: Guid = Guid([
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
]);
/// Microsoft basic data 的类型 GUID。
pub const BASIC_DATA_TYPE_GUID: Guid = Guid([
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
]);
/// 新 MBR 条目的默认类型字节：Linux。
pub const LINUX_MBR_TYPE: u8 = 0x83;

/// MBR 的 StartLBA / SizeInLBA 能表示的最大 LBA。
pub const MBR_LBA_MAX: u64 = u32::MAX as u64;

/// 扩展容器类型；本实现不管理逻辑分区，这三个字节在读写两侧都按结构语义对待。
pub const MBR_CONTAINER_TYPES: [u8; 3] = [TYPE_EXTENDED, TYPE_EXTENDED_LBA, TYPE_EXTENDED_LINUX];

/// MBR 的可见/隐藏类型码对照；映射取自 util-linux pt-mbr-partnames.h。
pub const MBR_HIDDEN_PAIRS: &[(u8, u8)] = &[
    (0x01, 0x11),
    (0x04, 0x14),
    (0x06, 0x16),
    (0x07, 0x17),
    (0x0B, 0x1B),
    (0x0C, 0x1C),
    (0x0E, 0x1E),
];

/// 一次编辑为什么不能进行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// 没有这个分区号。
    PartitionMissing(u32),
    /// 分区号指向空槽。
    PartitionEmpty(u32),
    /// 指定的槽位超出表能容纳的范围。
    SlotOutOfRange { number: u32, max: u32 },
    /// 指定的槽位已被占用。
    SlotOccupied(u32),
    /// 表里没有空槽。
    TableFull,
    /// 没有对齐后能完整容纳请求的空洞。
    NoGapFits { sectors: u64 },
    /// 区间越出可用范围。
    OutsideUsable {
        start: u64,
        end: u64,
        first: u64,
        last: u64,
    },
    /// 区间越过地址空间或 MBR 的 32 位 LBA 上限。
    PastAddressSpace {
        start: u64,
        sectors: u64,
        limit: u64,
    },
    /// 区间越出盘面。
    OutOfRange {
        start: u64,
        end: u64,
        first: u64,
        last: u64,
    },
    /// 区间与既有分区重叠。
    Overlaps {
        start: u64,
        end: u64,
        existing_start: u64,
        existing_end: u64,
    },
    /// 零长度分区不是可落位的对象。
    ZeroSize,
    /// 类型 GUID 全零是"未用条目"的形状。
    ZeroTypeGuid,
    /// MBR 类型 0x00 是空槽标记。
    ZeroTypeByte,
    /// 0xEE 是 GPT 保护记录，不是可描述的数据分区。
    ProtectiveTypeByte,
    /// 扩展容器类型不在本实现的范围内。
    ContainerTypeByte(u8),
    /// 没有对应的隐藏类型码。
    NoHiddenCounterpart(u8),
    /// 已经是目标状态，无需写入。
    AlreadyApplied,
    /// 不可用作编辑起点的表状态。
    UnusableTable(&'static str),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PartitionMissing(number) => write!(f, "partition {number} does not exist"),
            Self::PartitionEmpty(number) => write!(f, "partition {number} is empty"),
            Self::SlotOutOfRange { number, max } => {
                write!(f, "partition number {number} is outside 1..={max}")
            }
            Self::SlotOccupied(number) => {
                write!(f, "partition {number} is already in use — pick an empty slot")
            }
            Self::TableFull => f.write_str("the partition table has no empty slot"),
            Self::NoGapFits { sectors } => write!(
                f,
                "no aligned free gap fits {sectors} sectors — the target has no room for this partition"
            ),
            Self::OutsideUsable {
                start,
                end,
                first,
                last,
            } => write!(
                f,
                "range {start}..{end} is outside the usable range {first}..{last}"
            ),
            Self::PastAddressSpace {
                start,
                sectors,
                limit,
            } => write!(
                f,
                "a partition of {sectors} sectors starting at LBA {start} exceeds the expressible range (last LBA {limit})"
            ),
            Self::OutOfRange {
                start,
                end,
                first,
                last,
            } => write!(f, "range {start}..{end} is outside the disk range {first}..{last}"),
            Self::Overlaps {
                start,
                end,
                existing_start,
                existing_end,
            } => write!(
                f,
                "range {start}..{end} overlaps the partition at LBA {existing_start}..{existing_end}"
            ),
            Self::ZeroSize => f.write_str("a partition needs a size above zero"),
            Self::ZeroTypeGuid => f.write_str(
                "type GUID cannot be all-zero (that marks an unused entry — use `delete`)",
            ),
            Self::ZeroTypeByte => {
                f.write_str("type 0x00 marks an empty entry — use `delete` to remove the partition")
            }
            Self::ProtectiveTypeByte => f.write_str(
                "type 0xEE is the GPT protective record, not a data partition — use a real partition type",
            ),
            Self::ContainerTypeByte(byte) => write!(
                f,
                "extended-container type 0x{byte:02X} is out of scope — logical partitions are not supported"
            ),
            Self::NoHiddenCounterpart(byte) => write!(
                f,
                "type 0x{byte:02X} has no hidden counterpart (supported: fat12/16/32 incl. lba, ntfs)"
            ),
            Self::AlreadyApplied => f.write_str("the entry already has the requested value"),
            Self::UnusableTable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for EditError {}

/// GPT 属性旗标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GptFlag {
    /// ESP 类型 GUID 的开关。
    Esp,
    /// Legacy BIOS Bootable 属性位。
    Legacy,
    Hidden,
    Required,
}

impl GptFlag {
    /// 接受的写法。
    pub const NAMES: &'static str = "esp/boot, legacy/legacy_boot, hidden, required";

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "esp" | "boot" => Some(Self::Esp),
            "legacy" | "legacy_boot" => Some(Self::Legacy),
            "hidden" => Some(Self::Hidden),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    /// 该旗标改的是类型 GUID 还是属性位。
    pub const fn switches_type(self) -> bool {
        matches!(self, Self::Esp)
    }

    /// 属性位；ESP 走类型 GUID，不占属性位。
    pub const fn attribute_bit(self) -> u64 {
        match self {
            Self::Esp => 0,
            Self::Legacy => 1 << 2,
            Self::Hidden => 1 << 1,
            Self::Required => 1 << 0,
        }
    }
}

/// MBR 主分区旗标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MbrFlag {
    Boot,
    Hidden,
}

impl MbrFlag {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "boot" => Some(Self::Boot),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }

    /// 接受的写法。
    pub const NAMES: &'static str = "boot, hidden";
}

/// 一份可编辑的 GPT 表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptTable {
    pub header: GptHeader,
    pub entries: Vec<PartitionEntry>,
    pub sector_size: u64,
    pub total_bytes: u64,
}

impl GptTable {
    /// 容器的最后一个 LBA。
    pub fn last_lba(&self) -> u64 {
        self.total_bytes / self.sector_size - 1
    }

    /// 已定义条目的区间，按条目顺序。
    pub fn used_ranges(&self) -> Vec<(u64, u64)> {
        self.entries
            .iter()
            .filter(|entry| !entry.partition_type_guid.is_zero())
            .map(|entry| (entry.starting_lba, entry.ending_lba))
            .collect()
    }

    /// 已定义分区号（1 起），按条目顺序。
    pub fn live_numbers(&self) -> Vec<u32> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.partition_type_guid.is_zero())
            .map(|(index, _)| u32::try_from(index + 1).unwrap_or(u32::MAX))
            .collect()
    }

    /// `:last` 的落点：末端最靠后的已定义分区。
    pub fn last_live(&self) -> Option<u32> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.partition_type_guid.is_zero())
            .max_by_key(|(_, entry)| entry.ending_lba)
            .map(|(index, _)| u32::try_from(index + 1).unwrap_or(u32::MAX))
    }

    /// 1 起的已定义分区号 → 0 起的条目下标。
    pub fn live_index(&self, number: u32) -> Result<usize, EditError> {
        let Some(entry) = number
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.entries.get(index))
        else {
            return Err(EditError::PartitionMissing(number));
        };
        if entry.partition_type_guid.is_zero() {
            return Err(EditError::PartitionEmpty(number));
        }
        Ok(usize::try_from(number - 1).expect("上面已确认可表示"))
    }

    /// 新增条目的槽位：显式槽位须在范围内且为空，否则取第一个空槽。
    pub fn add_slot(&self, slot: Option<u32>, start: u64, end: u64) -> Result<usize, EditError> {
        let first = self.header.first_usable_lba;
        let last = self.header.last_usable_lba;
        if start > end || start < first || end > last {
            return Err(EditError::OutsideUsable {
                start,
                end,
                first,
                last,
            });
        }
        if let Some((existing_start, existing_end)) = self
            .used_ranges()
            .into_iter()
            .find(|(used_start, used_end)| !(end < *used_start || start > *used_end))
        {
            return Err(EditError::Overlaps {
                start,
                end,
                existing_start,
                existing_end,
            });
        }
        match slot {
            Some(number) => {
                let index = number
                    .checked_sub(1)
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|index| *index < self.entries.len())
                    .ok_or(EditError::SlotOutOfRange {
                        number,
                        max: u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
                    })?;
                if !self.entries[index].partition_type_guid.is_zero() {
                    return Err(EditError::SlotOccupied(number));
                }
                Ok(index)
            }
            None => self
                .entries
                .iter()
                .position(|entry| entry.partition_type_guid.is_zero())
                .ok_or(EditError::TableFull),
        }
    }

    /// 把一组条目序列化成双副本写段；顺序与建表一致：备份数组、备份头、主数组、主头。
    pub fn segments(&self, entries: &[PartitionEntry]) -> Result<Vec<Segment>, TableError> {
        let count = self.header.number_of_partition_entries;
        let entry_size = self.header.size_of_partition_entry;
        let sector = self.sector_size;
        let last_lba = self.last_lba();
        let span = entry_array_span(count, entry_size, sector)?;
        let backup_array_lba = last_lba
            .checked_sub(span)
            .ok_or(TableError::InvalidGeometry("no room for the backup array"))?;
        let logical = (count as usize)
            .checked_mul(entry_size as usize)
            .ok_or(TableError::InvalidGeometry("entry array size overflow"))?;
        let array = serialize_entries(entries, count, entry_size, sector as usize)?;
        let crc = array_crc(&array[..logical]);
        let primary = GptHeader {
            primary_lba: PRIMARY_HEADER_LBA,
            backup_lba: last_lba,
            partition_entry_lba: PRIMARY_ARRAY_LBA,
            entry_array_crc32: crc,
            ..self.header.clone()
        };
        let backup = GptHeader {
            primary_lba: last_lba,
            backup_lba: PRIMARY_HEADER_LBA,
            partition_entry_lba: backup_array_lba,
            entry_array_crc32: crc,
            ..self.header.clone()
        };
        let primary_header = primary.serialize(sector as usize)?;
        let backup_header = backup.serialize(sector as usize)?;
        Ok(vec![
            Segment {
                region: region(backup_array_lba * sector, array.len() as u64)?,
                bytes: array.clone(),
            },
            Segment {
                region: region(last_lba * sector, sector)?,
                bytes: backup_header,
            },
            Segment {
                region: region(PRIMARY_ARRAY_LBA * sector, array.len() as u64)?,
                bytes: array,
            },
            Segment {
                region: region(PRIMARY_HEADER_LBA * sector, sector)?,
                bytes: primary_header,
            },
        ])
    }
}

/// 按字节区间构造 `Region`；参数来自已验证的几何，溢出仍是畸形输入。
fn region(start: u64, len: u64) -> Result<Region, TableError> {
    Region::sized(start, len).map_err(|_| TableError::InvalidGeometry("region overflows"))
}

/// 新增条目的落地：0 属性位、空名字，名字由调用方先行校验。
pub fn new_entry(
    start: u64,
    end: u64,
    type_guid: Guid,
    unique: Guid,
    name: &str,
) -> PartitionEntry {
    PartitionEntry {
        partition_type_guid: type_guid,
        unique_partition_guid: unique,
        starting_lba: start,
        ending_lba: end,
        attributes: 0,
        name: PartitionName::from_str_lossy(name),
    }
}

/// 自动放置：第一个按 `unit` 对齐且完整容纳 `sectors` 的空洞。
///
/// `unit == 1` 表示不额外对齐。空洞两端都按 `unit` 收敛到可用范围内。
pub fn first_fit(
    used: &[(u64, u64)],
    first: u64,
    last: u64,
    unit: u64,
    sectors: u64,
) -> Result<(u64, u64), EditError> {
    if sectors == 0 {
        return Err(EditError::ZeroSize);
    }
    let unit = unit.max(1);
    for (gap_start, gap_end) in gaps(used, first, last, unit) {
        if gap_end - gap_start + 1 >= sectors {
            let start = gap_start;
            let end = start
                .checked_add(sectors - 1)
                .ok_or(EditError::PastAddressSpace {
                    start,
                    sectors,
                    limit: u64::MAX,
                })?;
            return Ok((start, end));
        }
    }
    Err(EditError::NoGapFits { sectors })
}

/// 显式起点：按 `unit` 上取整后落位；越界或与既有分区重叠即拒绝。
pub fn fixed_start(
    used: &[(u64, u64)],
    first: u64,
    last: u64,
    unit: u64,
    sectors: u64,
    requested: u64,
) -> Result<(u64, u64), EditError> {
    if sectors == 0 {
        return Err(EditError::ZeroSize);
    }
    let unit = unit.max(1);
    let start = requested
        .div_ceil(unit)
        .checked_mul(unit)
        .ok_or(EditError::PastAddressSpace {
            start: requested,
            sectors,
            limit: u64::MAX,
        })?;
    let end = start
        .checked_add(sectors - 1)
        .ok_or(EditError::PastAddressSpace {
            start,
            sectors,
            limit: u64::MAX,
        })?;
    if start < first || end > last {
        return Err(EditError::OutsideUsable {
            start,
            end,
            first,
            last,
        });
    }
    if let Some((existing_start, existing_end)) = used
        .iter()
        .copied()
        .find(|(used_start, used_end)| !(end < *used_start || start > *used_end))
    {
        return Err(EditError::Overlaps {
            start,
            end,
            existing_start,
            existing_end,
        });
    }
    Ok((start, end))
}

/// 对齐后的空闲区间，含两端。
fn gaps(used: &[(u64, u64)], first: u64, last: u64, unit: u64) -> Vec<(u64, u64)> {
    let mut sorted: Vec<(u64, u64)> = used
        .iter()
        .copied()
        .filter(|(start, end)| end >= start)
        .collect();
    sorted.sort_unstable();
    let mut free = Vec::new();
    let mut cursor = first;
    for (start, end) in sorted {
        if start > cursor {
            push_gap(&mut free, cursor, start - 1, unit);
        }
        cursor = cursor.max(end.saturating_add(1));
    }
    if cursor <= last {
        push_gap(&mut free, cursor, last, unit);
    }
    free
}

fn push_gap(free: &mut Vec<(u64, u64)>, start: u64, end: u64, unit: u64) {
    let Some(aligned_start) = start.div_ceil(unit).checked_mul(unit) else {
        return;
    };
    let Some(aligned_end) = end
        .checked_add(1)
        .and_then(|value| value.checked_div(unit))
        .and_then(|value| value.checked_mul(unit))
        .and_then(|value| value.checked_sub(1))
    else {
        return;
    };
    if aligned_start <= aligned_end {
        free.push((aligned_start, aligned_end));
    }
}

/// 把一条 GPT 条目改成目标旗标；`fallback` 只在关闭 ESP 时取用。
pub fn apply_gpt_flag(
    entry: &mut PartitionEntry,
    flag: GptFlag,
    on: bool,
    fallback: Option<Guid>,
) -> Result<(), EditError> {
    if flag.switches_type() {
        if on {
            entry.partition_type_guid = ESP_TYPE_GUID;
            return Ok(());
        }
        if entry.partition_type_guid == ESP_TYPE_GUID {
            let fallback = fallback.ok_or(EditError::UnusableTable(
                "the ESP type fallback was not resolved while planning",
            ))?;
            entry.partition_type_guid = fallback;
        }
        return Ok(());
    }
    let bit = flag.attribute_bit();
    if on {
        entry.attributes |= bit;
    } else {
        entry.attributes &= !bit;
    }
    Ok(())
}

/// 在 LBA0 上写入一条主分区记录，返回整扇区新内容与 1 起槽位号。
pub fn add_mbr(
    lba0: &[u8],
    slot: Option<u32>,
    start: u64,
    end: u64,
    os_type: u8,
    total_lba: u64,
) -> Result<(Vec<u8>, u32), EditError> {
    check_mbr_add_type(os_type)?;
    if total_lba < 2 {
        return Err(EditError::OutOfRange {
            start,
            end,
            first: 1,
            last: 0,
        });
    }
    if start < 1 || start > end || end >= total_lba {
        return Err(EditError::OutOfRange {
            start,
            end,
            first: 1,
            last: total_lba - 1,
        });
    }
    if end > MBR_LBA_MAX {
        return Err(EditError::PastAddressSpace {
            start,
            sectors: end - start + 1,
            limit: MBR_LBA_MAX,
        });
    }
    let mut out = lba0.to_vec();
    let mut first_empty = None;
    for index in 0..4usize {
        let record = mbr_record(&out, index)?;
        if record.part_type != 0 && record.size_lba == 0 {
            return Err(EditError::UnusableTable(
                "an existing MBR entry has a type byte but zero sectors; `delete` clears it",
            ));
        }
        if record.part_type == 0 {
            first_empty.get_or_insert(index);
            continue;
        }
        let used_start = u64::from(record.start_lba);
        let used_end = used_start
            .saturating_add(u64::from(record.size_lba))
            .saturating_sub(1);
        if !(end < used_start || start > used_end) {
            return Err(EditError::Overlaps {
                start,
                end,
                existing_start: used_start,
                existing_end: used_end,
            });
        }
    }
    let index = match slot {
        Some(number) => {
            let index = usize::try_from(number)
                .ok()
                .and_then(|number| number.checked_sub(1))
                .filter(|index| *index < 4)
                .ok_or(EditError::SlotOutOfRange { number, max: 4 })?;
            if out[MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES + 4] != 0 {
                return Err(EditError::SlotOccupied(number));
            }
            index
        }
        None => first_empty.ok_or(EditError::TableFull)?,
    };
    let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
    let record = &mut out[offset..offset + MBR_ENTRY_BYTES];
    record.fill(0);
    record[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    record[4] = os_type;
    record[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    record[8..12].copy_from_slice(&(start as u32).to_le_bytes());
    record[12..16].copy_from_slice(&((end - start + 1) as u32).to_le_bytes());
    out[MBR_SIGNATURE_OFFSET] = 0x55;
    out[MBR_SIGNATURE_OFFSET + 1] = 0xAA;
    Ok((out, u32::try_from(index + 1).expect("槽位 1..=4")))
}

/// 在 LBA0 上清掉一条主分区记录。
pub fn delete_mbr(lba0: &[u8], number: u32) -> Result<Vec<u8>, EditError> {
    primary_slot(number)?;
    let index = slot_index(number);
    let record = mbr_record(lba0, index)?;
    let current = record.part_type;
    if current == 0 {
        return Err(EditError::PartitionEmpty(number));
    }
    if current == TYPE_PROTECTIVE {
        return Err(EditError::ProtectiveTypeByte);
    }
    if MBR_CONTAINER_TYPES.contains(&current) {
        return Err(EditError::ContainerTypeByte(current));
    }
    let mut out = lba0.to_vec();
    let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
    out[offset..offset + MBR_ENTRY_BYTES].fill(0);
    Ok(out)
}

/// 改一条 MBR 记录的类型字节。
pub fn set_mbr_type(lba0: &[u8], number: u32, os_type: u8) -> Result<Vec<u8>, EditError> {
    primary_slot(number)?;
    check_mbr_add_type(os_type)?;
    let index = slot_index(number);
    let current = require_live_mbr(lba0, number, index)?;
    if current == TYPE_PROTECTIVE {
        return Err(EditError::ProtectiveTypeByte);
    }
    if MBR_CONTAINER_TYPES.contains(&current) {
        return Err(EditError::ContainerTypeByte(current));
    }
    let mut out = lba0.to_vec();
    out[MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES + 4] = os_type;
    Ok(out)
}

/// 置位/清除 boot 标志；置位时清掉其他三条记录的同一字节。
pub fn set_mbr_boot(lba0: &[u8], number: u32, on: bool) -> Result<Vec<u8>, EditError> {
    primary_slot(number)?;
    let index = slot_index(number);
    let current = require_live_mbr(lba0, number, index)?;
    // 扩展容器记录不是可引导分区：置 boot 只对主分区成立（参考 libfdisk 同口径拒绝）。
    if MBR_CONTAINER_TYPES.contains(&current) {
        return Err(EditError::ContainerTypeByte(current));
    }
    let mut out = lba0.to_vec();
    if on {
        for other in 0..4usize {
            out[MBR_ENTRY_BASE + other * MBR_ENTRY_BYTES] = 0;
        }
    }
    out[MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES] = if on { 0x80 } else { 0x00 };
    Ok(out)
}

/// 改一条 MBR 主分区记录的起止；起点与长度原样落位，不改类型与 boot 标志。
///
/// 只覆盖主分区 1..4：扩展容器与逻辑分区不在收缩/扩容范围内。
pub fn resize_mbr(
    lba0: &[u8],
    number: u32,
    start: u64,
    end: u64,
    total_lba: u64,
) -> Result<Vec<u8>, EditError> {
    primary_slot(number)?;
    let index = slot_index(number);
    let current = require_live_mbr(lba0, number, index)?;
    if MBR_CONTAINER_TYPES.contains(&current) {
        return Err(EditError::ContainerTypeByte(current));
    }
    if current == TYPE_PROTECTIVE {
        return Err(EditError::ProtectiveTypeByte);
    }
    if total_lba < 2 || start < 1 || start > end || end >= total_lba {
        return Err(EditError::OutOfRange {
            start,
            end,
            first: 1,
            last: total_lba.saturating_sub(1),
        });
    }
    if end > MBR_LBA_MAX {
        return Err(EditError::PastAddressSpace {
            start,
            sectors: end - start + 1,
            limit: MBR_LBA_MAX,
        });
    }
    // 邻接自守（与 add_mbr 同判据）：其余槽位任一与新区间重叠即拒绝，且必须挡在
    // 返回新 LBA0 之前——决定层在写盘前只凭这里的结果放行。
    for other in 0..4usize {
        if other == index {
            continue;
        }
        let record = mbr_record(lba0, other)?;
        if record.part_type == 0 || record.size_lba == 0 {
            continue;
        }
        let used_start = u64::from(record.start_lba);
        let used_end = used_start
            .saturating_add(u64::from(record.size_lba))
            .saturating_sub(1);
        if !(end < used_start || start > used_end) {
            return Err(EditError::Overlaps {
                start,
                end,
                existing_start: used_start,
                existing_end: used_end,
            });
        }
    }
    let mut out = lba0.to_vec();
    let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
    out[offset + 8..offset + 12].copy_from_slice(&(start as u32).to_le_bytes());
    out[offset + 12..offset + 16].copy_from_slice(&((end - start + 1) as u32).to_le_bytes());
    Ok(out)
}

/// 切换 visible ↔ hidden 类型码；已在目标态时返回 `None`。
pub fn set_mbr_hidden(lba0: &[u8], number: u32, on: bool) -> Result<Option<Vec<u8>>, EditError> {
    primary_slot(number)?;
    let index = slot_index(number);
    let current = require_live_mbr(lba0, number, index)?;
    if MBR_HIDDEN_PAIRS.iter().any(|&(visible, hidden)| {
        if on {
            hidden == current
        } else {
            visible == current
        }
    }) {
        return Ok(None);
    }
    let next = hidden_counterpart(current, on).ok_or(EditError::NoHiddenCounterpart(current))?;
    let mut out = lba0.to_vec();
    out[MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES + 4] = next;
    Ok(Some(out))
}

/// MBR 主分区记录的占用区间；扩展容器按到盘尾计算。
pub fn mbr_used_ranges(lba0: &[u8], total_lba: u64) -> Result<Vec<(u64, u64)>, EditError> {
    let mut used = Vec::new();
    for index in 0..4usize {
        let record = mbr_record(lba0, index)?;
        if record.part_type == 0 {
            continue;
        }
        let start = u64::from(record.start_lba);
        let end = if MBR_CONTAINER_TYPES.contains(&record.part_type) {
            total_lba.saturating_sub(1)
        } else {
            start
                .saturating_add(u64::from(record.size_lba))
                .saturating_sub(1)
        };
        used.push((start, end));
    }
    Ok(used)
}

/// MBR 的 `:last`：末端最靠后的主分区，扩展容器除外。
pub fn mbr_last_live(lba0: &[u8]) -> Result<Option<u32>, EditError> {
    let mut best: Option<(u64, u32)> = None;
    for index in 0..4usize {
        let record = mbr_record(lba0, index)?;
        if record.part_type == 0 || MBR_CONTAINER_TYPES.contains(&record.part_type) {
            continue;
        }
        let end = u64::from(record.start_lba)
            .saturating_add(u64::from(record.size_lba))
            .saturating_sub(1);
        let number = u32::try_from(index + 1).expect("槽位 1..=4");
        if best.is_none_or(|(best_end, _)| end > best_end) {
            best = Some((end, number));
        }
    }
    Ok(best.map(|(_, number)| number))
}

fn check_mbr_add_type(os_type: u8) -> Result<(), EditError> {
    if os_type == 0 {
        return Err(EditError::ZeroTypeByte);
    }
    check_mbr_type_byte(os_type)
}

fn check_mbr_type_byte(os_type: u8) -> Result<(), EditError> {
    if os_type == TYPE_PROTECTIVE {
        return Err(EditError::ProtectiveTypeByte);
    }
    if MBR_CONTAINER_TYPES.contains(&os_type) {
        return Err(EditError::ContainerTypeByte(os_type));
    }
    Ok(())
}

fn primary_slot(number: u32) -> Result<(), EditError> {
    if (1..=4).contains(&number) {
        Ok(())
    } else {
        Err(EditError::SlotOutOfRange { number, max: 4 })
    }
}

fn slot_index(number: u32) -> usize {
    usize::try_from(number - 1).expect("槽位 1..=4")
}

fn require_live_mbr(lba0: &[u8], number: u32, index: usize) -> Result<u8, EditError> {
    let record = mbr_record(lba0, index)?;
    if record.part_type == 0 {
        return Err(EditError::PartitionEmpty(number));
    }
    if record.size_lba == 0 {
        return Err(EditError::UnusableTable(
            "the entry has a type byte but zero sectors; `delete` clears it",
        ));
    }
    Ok(record.part_type)
}

fn mbr_record(lba0: &[u8], index: usize) -> Result<MbrEntry, EditError> {
    let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
    let Some(record) = lba0.get(offset..offset + MBR_ENTRY_BYTES) else {
        return Err(EditError::UnusableTable(
            "the MBR sector is shorter than 512 bytes",
        ));
    };
    Ok(MbrEntry {
        boot: record[0],
        part_type: record[4],
        start_lba: u32::from_le_bytes(record[8..12].try_into().expect("偏移已校验")),
        size_lba: u32::from_le_bytes(record[12..16].try_into().expect("偏移已校验")),
    })
}

fn hidden_counterpart(current: u8, hide: bool) -> Option<u8> {
    MBR_HIDDEN_PAIRS.iter().find_map(|&(visible, hidden)| {
        if hide && visible == current {
            Some(hidden)
        } else if !hide && hidden == current {
            Some(visible)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mbr_bytes(entries: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        for (index, &(part_type, start, size)) in entries.iter().enumerate() {
            let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
            sector[offset + 4] = part_type;
            sector[offset + 8..offset + 12].copy_from_slice(&start.to_le_bytes());
            sector[offset + 12..offset + 16].copy_from_slice(&size.to_le_bytes());
        }
        sector[MBR_SIGNATURE_OFFSET] = 0x55;
        sector[MBR_SIGNATURE_OFFSET + 1] = 0xAA;
        sector
    }

    #[test]
    fn resize_mbr_rewrites_start_and_size_in_place() {
        let lba0 = mbr_bytes(&[(0x83, 2048, 1024)]);
        let resized = resize_mbr(&lba0, 1, 2048, 4095, 8192).expect("主分区可扩");
        let record = mbr_record(&resized, 0).expect("记录可读");
        assert_eq!(record.part_type, 0x83, "类型字节保持不变");
        assert_eq!(record.start_lba, 2048);
        assert_eq!(record.size_lba, 2048);
        assert!(matches!(
            resize_mbr(&lba0, 1, 0, 100, 8192),
            Err(EditError::OutOfRange { .. })
        ));
        assert!(matches!(
            resize_mbr(&lba0, 1, 2048, 9000, 8192),
            Err(EditError::OutOfRange { .. })
        ));
    }

    #[test]
    fn first_fit_picks_the_first_aligned_gap() {
        let used = [(2048, 4095)];
        assert_eq!(first_fit(&used, 34, 8189, 2048, 1024), Ok((4096, 5119)));
        assert_eq!(
            first_fit(&used, 2048, 4095, 1, 10),
            Err(EditError::NoGapFits { sectors: 10 })
        );
    }

    #[test]
    fn fixed_start_aligns_and_rejects_overlap() {
        let used = [(2048, 4095)];
        assert_eq!(
            fixed_start(&used, 34, 8189, 2048, 10, 2100),
            Ok((4096, 4105))
        );
        assert!(matches!(
            fixed_start(&used, 34, 8189, 1, 10, 3000),
            Err(EditError::Overlaps { .. })
        ));
    }

    #[test]
    fn mbr_add_uses_the_first_empty_slot_and_rejects_overlap() {
        let lba0 = mbr_bytes(&[(0x83, 2048, 1024)]);
        let (out, number) = add_mbr(&lba0, None, 4096, 5119, 0x83, 8192).expect("应可新增");
        assert_eq!(number, 2);
        assert_eq!(out[MBR_ENTRY_BASE + 16 + 4], 0x83);
        // CHS 两字段写 LBA-only 占位（gdisk 同值；fdisk/parted 在 >1023 柱面溢出钳位
        // 后也是同值）：起点 [FE FF FF]、终点 [FE FF FF]
        assert_eq!(
            &out[MBR_ENTRY_BASE + 16 + 1..MBR_ENTRY_BASE + 16 + 4],
            &[0xFE, 0xFF, 0xFF]
        );
        assert_eq!(
            &out[MBR_ENTRY_BASE + 16 + 5..MBR_ENTRY_BASE + 16 + 8],
            &[0xFE, 0xFF, 0xFF]
        );
        assert!(matches!(
            add_mbr(&out, None, 3000, 3500, 0x83, 8192),
            Err(EditError::Overlaps { .. })
        ));
    }

    #[test]
    fn mbr_delete_clears_only_the_record() {
        let lba0 = mbr_bytes(&[(0x83, 2048, 1024)]);
        let out = delete_mbr(&lba0, 1).expect("应可删除");
        assert_eq!(&out[MBR_ENTRY_BASE..MBR_ENTRY_BASE + 16], &[0u8; 16]);
        assert_eq!(&out[510..512], &[0x55, 0xAA]);
    }

    #[test]
    fn hidden_flag_round_trips_and_has_no_counterpart_for_linux() {
        let lba0 = mbr_bytes(&[(0x0C, 2048, 1024), (0x83, 4096, 1024)]);
        let hidden = set_mbr_hidden(&lba0, 1, true).expect("FAT32 LBA 有隐藏码");
        assert_eq!(
            hidden.as_ref().map(|bytes| bytes[MBR_ENTRY_BASE + 4]),
            Some(0x1C)
        );
        let visible = set_mbr_hidden(hidden.as_ref().expect("有内容"), 1, false).expect("应可还原");
        assert_eq!(
            visible.as_ref().map(|bytes| bytes[MBR_ENTRY_BASE + 4]),
            Some(0x0C)
        );
        assert_eq!(
            set_mbr_hidden(&lba0, 2, true),
            Err(EditError::NoHiddenCounterpart(0x83))
        );
    }

    #[test]
    fn boot_flag_on_an_extended_container_is_refused() {
        let lba0 = mbr_bytes(&[(TYPE_EXTENDED, 2048, 1024)]);
        assert_eq!(
            set_mbr_boot(&lba0, 1, true),
            Err(EditError::ContainerTypeByte(TYPE_EXTENDED))
        );
    }

    #[test]
    fn mbr_boot_flag_moves_between_slots() {
        let lba0 = mbr_bytes(&[(0x83, 2048, 1024), (0x83, 4096, 1024)]);
        let with_boot = set_mbr_boot(&lba0, 2, true).expect("应可置位");
        assert_eq!(with_boot[MBR_ENTRY_BASE], 0);
        assert_eq!(with_boot[MBR_ENTRY_BASE + 16], 0x80);
    }
}
