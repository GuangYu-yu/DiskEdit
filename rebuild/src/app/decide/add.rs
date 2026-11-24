//! `add`：在既有分区表里新增一个分区。
//!
//! 只做表区校验与计划；分区上限、可用区间与重叠都在这里判定。起点、对齐与类型
//! 允许省略，省略时按固定规则补齐：对齐取 1 MiB，类型取 Linux 文件系统数据。

use diskedit_core::table::Guid;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{AddRequest, Snapshot, TableFormat};

/// 省略 `--align` 时的对齐字节数。
pub const DEFAULT_ALIGN_BYTES: u64 = 1 << 20;

/// 省略 `--type-guid` 时使用的 GPT 类型：Linux 文件系统数据。
pub const DEFAULT_GPT_TYPE: Guid = Guid([
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
]);

pub fn plan(snapshot: &Snapshot, request: &AddRequest) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let sectors = request.size.div_ceil(admitted.sector);
    if sectors == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "partition length is zero".to_owned(),
        });
    }
    let step = align_step(request.align, admitted.sector)?;
    let table = admission::table(snapshot)?;
    let steps = match table {
        admission::TableView::Gpt(copy) => {
            let header = &copy.header;
            let start_lba = match request.start {
                Some(start) => start,
                None => first_fit(
                    header.first_usable_lba,
                    header.last_usable_lba,
                    sectors,
                    step,
                    admission::gpt_used(copy)
                        .map(|(_, entry)| (entry.starting_lba, entry.ending_lba)),
                )?,
            };
            let end_lba =
                start_lba
                    .checked_add(sectors - 1)
                    .ok_or_else(|| Refusal::ArgumentOutOfRange {
                        what: "partition end exceeds the addressable bound".to_owned(),
                    })?;
            if start_lba < header.first_usable_lba || end_lba > header.last_usable_lba {
                return Err(Refusal::CapacityInsufficient {
                    needed: sectors.saturating_mul(admitted.sector),
                    available: usable_bytes(
                        header.first_usable_lba,
                        header.last_usable_lba,
                        admitted.sector,
                    ),
                });
            }
            for (number, entry) in admission::gpt_used(copy) {
                if admission::overlaps(
                    start_lba,
                    sectors,
                    entry.starting_lba,
                    entry.ending_lba - entry.starting_lba + 1,
                ) {
                    return Err(Refusal::PartitionOverlap { number });
                }
            }
            ensure_slot(
                copy.entries.len(),
                admission::gpt_free_slot(copy),
                request.part_number,
            )?;
            let layout = capacity::gpt_layout(admitted.length, admitted.sector)?;
            let regions = capacity::table_regions(TableFormat::Gpt, Some(layout), admitted.sector)?;
            vec![
                Step::new(
                    StepKind::AddPartition {
                        format: TableFormat::Gpt,
                        start_lba,
                        sectors,
                        type_guid: request.type_guid.unwrap_or(DEFAULT_GPT_TYPE),
                        name: request.name.clone().unwrap_or_default(),
                        part_number: request.part_number,
                        unique: None,
                    },
                    regions,
                ),
                Step::new(StepKind::RefreshKernelView, Vec::new()),
            ]
        }
        admission::TableView::Mbr(mbr) => {
            if mbr.extended().is_some() {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR extended partition".to_owned(),
                    why: "targets with extended partitions are not supported for add".to_owned(),
                });
            }
            let total = capacity::require_mbr_capacity(admitted.length, admitted.sector)?;
            let start_lba = match request.start {
                Some(start) => start,
                None => first_fit(
                    1,
                    total.saturating_sub(1),
                    sectors,
                    step,
                    admission::mbr_used(mbr).map(|(_, entry)| {
                        (
                            u64::from(entry.start_lba),
                            u64::from(entry.start_lba) + u64::from(entry.size_lba) - 1,
                        )
                    }),
                )?,
            };
            let start = u32::try_from(start_lba).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR start exceeds 32 bits".to_owned(),
            })?;
            let size = u32::try_from(sectors).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR length exceeds 32 bits".to_owned(),
            })?;
            if u64::from(start) + u64::from(size) > total {
                return Err(Refusal::CapacityInsufficient {
                    needed: sectors.saturating_mul(admitted.sector),
                    available: total.saturating_mul(admitted.sector),
                });
            }
            for (number, entry) in admission::mbr_used(mbr) {
                if admission::overlaps(
                    u64::from(start),
                    u64::from(size),
                    u64::from(entry.start_lba),
                    u64::from(entry.size_lba),
                ) {
                    return Err(Refusal::PartitionOverlap { number });
                }
            }
            ensure_slot(
                mbr.entries.len(),
                admission::mbr_free_slot(mbr),
                request.part_number,
            )?;
            let regions = capacity::table_regions(TableFormat::Mbr, None, admitted.sector)?;
            vec![
                Step::new(
                    StepKind::AddPartition {
                        format: TableFormat::Mbr,
                        start_lba: u64::from(start),
                        sectors: u64::from(size),
                        type_guid: request.type_guid.unwrap_or(Guid::ZERO),
                        name: request.name.clone().unwrap_or_default(),
                        part_number: request.part_number,
                        unique: None,
                    },
                    regions,
                ),
                Step::new(StepKind::RefreshKernelView, Vec::new()),
            ]
        }
    };
    Plan::write(steps, admitted.identity)
}

/// 把字节对齐换算成扇区对齐；0 或缺失按默认 1 MiB。
fn align_step(align: Option<u64>, sector: u64) -> Result<u64, Refusal> {
    let bytes = align.unwrap_or(DEFAULT_ALIGN_BYTES);
    if bytes == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "alignment cannot be 0".to_owned(),
        });
    }
    Ok(bytes.div_ceil(sector).max(1))
}

/// 在可用区间里找第一个放得下的对齐起点。
fn first_fit(
    base: u64,
    last: u64,
    sectors: u64,
    step: u64,
    used: impl Iterator<Item = (u64, u64)>,
) -> Result<u64, Refusal> {
    let mut intervals: Vec<(u64, u64)> = used.collect();
    intervals.sort_unstable();
    let mut candidate = align_up(base, step);
    for (start, end) in intervals {
        if candidate.saturating_add(sectors - 1) < start {
            break;
        }
        candidate = align_up(candidate.max(end.saturating_add(1)), step);
    }
    if candidate.saturating_add(sectors - 1) > last {
        return Err(Refusal::CapacityInsufficient {
            needed: sectors,
            available: last.saturating_sub(base).saturating_add(1),
        });
    }
    Ok(candidate)
}

fn align_up(value: u64, step: u64) -> u64 {
    value.div_ceil(step).saturating_mul(step)
}

fn ensure_slot(slots: usize, free: Option<u32>, requested: Option<u32>) -> Result<(), Refusal> {
    let Some(requested) = requested else {
        return free.map(|_| ()).ok_or(Refusal::CapacityInsufficient {
            needed: 1,
            available: 0,
        });
    };
    if usize::try_from(requested)
        .ok()
        .is_none_or(|index| index == 0 || index > slots)
    {
        return Err(Refusal::ArgumentOutOfRange {
            what: format!("partition number {requested} out of entry slot range"),
        });
    }
    if free.is_none() {
        return Err(Refusal::CapacityInsufficient {
            needed: 1,
            available: 0,
        });
    }
    Ok(())
}

fn usable_bytes(first: u64, last: u64, sector: u64) -> u64 {
    last.saturating_sub(first)
        .saturating_add(1)
        .saturating_mul(sector)
}
