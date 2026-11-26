//! 表编辑命令的现场：原始表字节、写前摘要、占用与记录。
//!
//! `add` / `delete` / `set` 的判定需要"当前表长什么样"，而表视图只留了渲染用的
//! 投影。这里把可编辑的原始表与渲染视图一起采齐：判定层据此算出新字节，执行层
//! 只消费由此生成的写段。

use diskedit_core::region::Region;
use diskedit_core::table::create::{PmbrSize, pmbr_size_state};
use diskedit_core::table::gpt::entries::PartitionEntry;
use diskedit_core::table::gpt::header::GptHeader;
use diskedit_core::table::gpt::parse::entry_array_span;
use diskedit_core::table::gpt::parse::{self, GptEvidence, GptState};
use diskedit_core::table::{PRIMARY_ARRAY_LBA, PRIMARY_HEADER_LBA};

use crate::platform::{Occupancy, Platform, TargetHandle};

use super::new::{PriorState, prior_state, read_region, stamp_of};
use super::records::RecordState;
use super::snapshot::{CaptureError, TargetFacts, open_target, record_state};
use super::table::{self, TableView};
use super::{Command, Request};

/// 可编辑的表事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableFacts {
    Gpt {
        header: GptHeader,
        entries: Vec<PartitionEntry>,
        sector_size: u64,
        total_bytes: u64,
        /// 解析出的副本状态；非 `Valid` 时写路径先在同一个表写里收敛它。
        state: GptState,
        /// 保护 MBR 的 SizeInLBA 结论。
        pmbr: PmbrSize,
        /// LBA0 的整扇区原字节，供保护 MBR 重写时保留前 446 字节。
        lba0: Vec<u8>,
    },
    Mbr {
        /// LBA0 的整段字节（至少 512 字节）。
        lba0: Vec<u8>,
        /// 容器的 LBA 数；MBR 的区间上界。
        total_lba: u64,
    },
}

/// `add` / `delete` / `set` 消费的现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditSnapshot {
    pub target: TargetFacts,
    pub occupancy: Occupancy,
    pub record: RecordState,
    pub prior: PriorState,
    pub view: TableView,
    /// 表不可编辑时为 `None`；此时 `view` 说明原因。
    pub facts: Option<TableFacts>,
    pub stamp: [u8; 32],
    pub is_partition_node: bool,
}

/// 采集一次编辑现场。
pub fn capture_edit(
    platform: &dyn Platform,
    request: &Request,
) -> Result<EditSnapshot, CaptureError> {
    if !matches!(
        request.command(),
        Command::Add(_)
            | Command::Delete { .. }
            | Command::Set(_)
            | Command::Move(_)
            | Command::Copy(_)
    ) {
        return Err(CaptureError::Unreadable {
            why: "capture_edit called for a different command".to_owned(),
        });
    }
    let target = open_target(platform, request)?;
    let handle = platform.open_target_read_only(&target.path)?;
    let view = table::observe(handle.as_ref(), request.sector_size(), handle.sector_size())
        .map_err(|error| CaptureError::Unreadable {
            why: error.to_string(),
        })?;
    let record = record_state(platform, &target)?;
    let occupancy = platform.occupancy(&target.path)?;
    let is_partition_node = platform.is_partition_node(&target.path);
    let sector_size = match &view {
        TableView::Gpt(view) => view.sector_size,
        TableView::Mbr(view) => view.sector_size,
        TableView::Damaged { sector_size, .. } => *sector_size,
        TableView::None => target.sector_size,
    };
    let stamp = stamp_of(handle.as_ref(), target.bytes, sector_size)?;
    let facts = match &view {
        TableView::Gpt(_) => Some(capture_gpt(handle.as_ref(), target.bytes, sector_size)?),
        TableView::Mbr(_) => Some(capture_mbr(handle.as_ref(), target.bytes, sector_size)?),
        TableView::Damaged { .. } | TableView::None => None,
    };
    let prior = prior_state(&record);
    Ok(EditSnapshot {
        target: TargetFacts {
            sector_size,
            ..target
        },
        occupancy,
        record,
        prior,
        view,
        facts,
        stamp,
        is_partition_node,
    })
}

pub(super) fn capture_gpt(
    handle: &dyn TargetHandle,
    total_bytes: u64,
    sector_size: u64,
) -> Result<TableFacts, CaptureError> {
    let last_lba = total_bytes / sector_size;
    if last_lba < 2 {
        return Err(CaptureError::Unreadable {
            why: "the target is too small for a GPT".to_owned(),
        });
    }
    let last_lba = last_lba - 1;
    let primary_header = read_region(handle, sector(PRIMARY_HEADER_LBA, sector_size))?;
    let backup_header = read_region(handle, sector(last_lba, sector_size))?;
    let shape = GptHeader::parse(&primary_header)
        .or_else(|_| GptHeader::parse(&backup_header))
        .map_err(|error| CaptureError::Unreadable {
            why: format!("GPT header: {error}"),
        })?;
    let span = entry_array_span(
        shape.number_of_partition_entries,
        shape.size_of_partition_entry,
        sector_size,
    )
    .map_err(|error| CaptureError::Unreadable {
        why: format!("GPT entry array: {error}"),
    })?;
    let array_bytes = span
        .checked_mul(sector_size)
        .ok_or_else(|| CaptureError::Unreadable {
            why: "GPT entry array size overflows".to_owned(),
        })?;
    let primary_array = read_region(
        handle,
        Region::sized(PRIMARY_ARRAY_LBA * sector_size, array_bytes).expect("区间自洽"),
    )?;
    let backup_array_lba = last_lba
        .checked_sub(span)
        .ok_or_else(|| CaptureError::Unreadable {
            why: "no room for the backup GPT entry array".to_owned(),
        })?;
    let backup_array = read_region(
        handle,
        Region::sized(backup_array_lba * sector_size, array_bytes).expect("区间自洽"),
    )?;
    let lba0 = read_region(
        handle,
        Region::sized(0, sector_size.max(512)).map_err(|_| CaptureError::Unreadable {
            why: "the protective MBR sector does not fit the target".to_owned(),
        })?,
    )?;
    match parse::classify(
        &primary_header,
        &primary_array,
        &backup_header,
        &backup_array,
        last_lba,
        sector_size,
    ) {
        GptEvidence::Usable(copy) => Ok(TableFacts::Gpt {
            header: copy.header,
            entries: copy.entries,
            sector_size,
            total_bytes,
            state: copy.state,
            pmbr: pmbr_size_state(&lba0, total_bytes, sector_size),
            lba0,
        }),
        GptEvidence::Damaged(error) => Err(CaptureError::Unreadable {
            why: format!("GPT: {error}"),
        }),
        GptEvidence::Absent => Err(CaptureError::Unreadable {
            why: "the GPT disappeared while it was being read".to_owned(),
        }),
    }
}

pub(super) fn capture_mbr(
    handle: &dyn TargetHandle,
    total_bytes: u64,
    sector_size: u64,
) -> Result<TableFacts, CaptureError> {
    let len = sector_size.max(512);
    let bytes = read_region(
        handle,
        Region::sized(0, len).map_err(|_| CaptureError::Unreadable {
            why: "the MBR sector does not fit the target".to_owned(),
        })?,
    )?;
    if !total_bytes.is_multiple_of(sector_size) {
        return Err(CaptureError::Unreadable {
            why: "the target length is not a whole number of sectors".to_owned(),
        });
    }
    Ok(TableFacts::Mbr {
        lba0: bytes,
        total_lba: total_bytes / sector_size,
    })
}

fn sector(lba: u64, sector_size: u64) -> Region {
    Region::sized(lba * sector_size, sector_size).expect("扇区区间自洽")
}
