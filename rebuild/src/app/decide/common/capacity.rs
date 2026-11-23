//! 分区表几何与最小容量。
//!
//! 全部是纯计算：只接收目标的字节长度与逻辑块大小，产出可用区间和需要写入
//! 的区间集合。容量不足与几何越界都在这里拒绝。

use diskedit_core::region::{Region, RegionError};
use diskedit_core::table::gpt::entries::is_used;
use diskedit_core::table::gpt::parse::GptCopy;
use diskedit_core::table::Mbr;

use super::super::TableFormat;
use super::Refusal;

/// 一份 GPT 条目数组的字节数。
pub const ENTRY_ARRAY_BYTES: u64 = 128 * 128;
/// 保护性 MBR 与 MBR 自身的字节数。
const MBR_BYTES: u64 = 512;

/// GPT 在给定容器上的布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GptLayout {
    pub total_lba: u64,
    pub array_span: u64,
    pub first_usable: u64,
    pub last_usable: u64,
}

impl GptLayout {
    pub fn backup_header_lba(self) -> u64 {
        self.total_lba - 1
    }

    pub fn backup_array_lba(self) -> u64 {
        self.total_lba - 1 - self.array_span
    }
}

/// 计算 GPT 布局；容器放不下头部与两份条目数组时拒绝。
pub fn gpt_layout(length: u64, sector: u64) -> Result<GptLayout, Refusal> {
    if sector == 0 {
        return Err(Refusal::GeometryUnknown {
            why: "逻辑块大小为零".to_owned(),
        });
    }
    let total_lba = length / sector;
    let array_span = ENTRY_ARRAY_BYTES.div_ceil(sector);
    // 需要 LBA0、主头、主数组与备份头、备份数组各自的空间。
    let minimum = 2 * array_span + 4;
    if total_lba < minimum {
        return Err(Refusal::CapacityInsufficient {
            needed: minimum * sector,
            available: length,
        });
    }
    let first_usable = 2 + array_span;
    let last_usable = total_lba - array_span - 2;
    if first_usable > last_usable {
        return Err(Refusal::CapacityInsufficient {
            needed: minimum * sector,
            available: length,
        });
    }
    Ok(GptLayout {
        total_lba,
        array_span,
        first_usable,
        last_usable,
    })
}

/// MBR 的最小容量：至少三个逻辑块才能让首个分区有位置。
pub fn require_mbr_capacity(length: u64, sector: u64) -> Result<u64, Refusal> {
    if sector == 0 {
        return Err(Refusal::GeometryUnknown {
            why: "逻辑块大小为零".to_owned(),
        });
    }
    let total_lba = length / sector;
    if total_lba < 3 {
        return Err(Refusal::CapacityInsufficient {
            needed: 3 * sector,
            available: length,
        });
    }
    Ok(total_lba)
}

/// 一次要写入的区间集合。
pub fn table_regions(
    format: TableFormat,
    layout: Option<GptLayout>,
    sector: u64,
) -> Result<Vec<Region>, Refusal> {
    match format {
        TableFormat::Mbr => Ok(vec![region(0, MBR_BYTES)?]),
        TableFormat::Gpt => {
            let layout = layout.ok_or_else(|| Refusal::GeometryUnknown {
                why: "缺少 GPT 布局".to_owned(),
            })?;
            let span_bytes = layout.array_span * sector;
            Ok(vec![
                region(0, MBR_BYTES)?,
                region(sector, sector)?,
                region(2 * sector, span_bytes)?,
                region(
                    layout.backup_array_lba() * sector,
                    span_bytes,
                )?,
                region(layout.backup_header_lba() * sector, sector)?,
            ])
        }
    }
}

/// 分区的字节区间。
pub fn partition_region(start_lba: u64, sectors: u64, sector: u64) -> Result<Region, Refusal> {
    let offset = start_lba
        .checked_mul(sector)
        .ok_or_else(|| Refusal::GeometryUnknown {
            why: "分区起点越过可寻址上界".to_owned(),
        })?;
    let length = sectors
        .checked_mul(sector)
        .ok_or_else(|| Refusal::GeometryUnknown {
            why: "分区长度越过可寻址上界".to_owned(),
        })?;
    region(offset, length)
}

fn region(offset: u64, length: u64) -> Result<Region, Refusal> {
    Region::sized(offset, length).map_err(|error: RegionError| Refusal::GeometryUnknown {
        why: format!("区间无法构造：{error:?}"),
    })
}
