//! `add`：在既有分区表里新增一个分区。
//!
//! 只做表区校验与计划；分区上限、可用区间与重叠都在这里判定。

use diskedit_core::table::Guid;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;
use crate::app::observe::TableFormat;

pub fn plan(
    snapshot: &Snapshot,
    start_lba: u64,
    sectors: u64,
    type_guid: Guid,
    name: String,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    if sectors == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "分区长度为零".to_owned(),
        });
    }
    let table = admission::table(snapshot)?;
    let steps = match table {
        admission::TableView::Gpt(copy) => {
            let header = &copy.header;
            let end_lba =
                start_lba
                    .checked_add(sectors - 1)
                    .ok_or_else(|| Refusal::ArgumentOutOfRange {
                        what: "分区终点越过可寻址上界".to_owned(),
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
            if admission::gpt_free_slot(copy).is_none() {
                return Err(Refusal::CapacityInsufficient {
                    needed: 1,
                    available: 0,
                });
            }
            let layout = capacity::gpt_layout(admitted.length, admitted.sector)?;
            let regions = capacity::table_regions(TableFormat::Gpt, Some(layout), admitted.sector)?;
            vec![
                Step::new(
                    StepKind::AddPartition {
                        format: TableFormat::Gpt,
                        start_lba,
                        sectors,
                        type_guid,
                        name,
                    },
                    regions,
                ),
                Step::new(StepKind::RefreshKernelView, Vec::new()),
            ]
        }
        admission::TableView::Mbr(mbr) => {
            if mbr.extended().is_some() {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR 扩展分区".to_owned(),
                    why: "含扩展分区的目标不参与新增".to_owned(),
                });
            }
            let start = u32::try_from(start_lba).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR 起点超过 32 位".to_owned(),
            })?;
            let size = u32::try_from(sectors).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR 长度超过 32 位".to_owned(),
            })?;
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
            if admission::mbr_free_slot(mbr).is_none() {
                return Err(Refusal::CapacityInsufficient {
                    needed: 1,
                    available: 0,
                });
            }
            let regions = capacity::table_regions(TableFormat::Mbr, None, admitted.sector)?;
            vec![
                Step::new(
                    StepKind::AddPartition {
                        format: TableFormat::Mbr,
                        start_lba,
                        sectors,
                        type_guid,
                        name,
                    },
                    regions,
                ),
                Step::new(StepKind::RefreshKernelView, Vec::new()),
            ]
        }
    };
    Plan::write(steps, admitted.identity)
}

fn usable_bytes(first: u64, last: u64, sector: u64) -> u64 {
    last.saturating_sub(first)
        .saturating_add(1)
        .saturating_mul(sector)
}
