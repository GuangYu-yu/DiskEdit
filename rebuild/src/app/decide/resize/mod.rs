//! `resize`：改变分区在表里的起止。
//!
//! 收缩需要先证明文件系统已缩到新范围；当前快照不含该事实，收缩按未知拒绝，
//! 不把"没读到"当作"已经缩好"。

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;
use crate::app::observe::TableFormat;

pub fn plan(snapshot: &Snapshot, partition: u32, sectors: u64) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let facts = admission::partition(snapshot, partition)?;
    if sectors == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "分区长度为零".to_owned(),
        });
    }
    if sectors < facts.partition.sectors {
        return Err(Refusal::FactUnknown {
            what: "filesystem used bytes".to_owned(),
        });
    }
    let table = admission::table(snapshot)?;
    let format = table.format();
    let start = facts.partition.start_lba;
    let end_lba = start
        .checked_add(sectors - 1)
        .ok_or_else(|| Refusal::ArgumentOutOfRange {
            what: "分区终点越过可寻址上界".to_owned(),
        })?;
    match table {
        admission::TableView::Gpt(copy) => {
            if start < copy.header.first_usable_lba || end_lba > copy.header.last_usable_lba {
                return Err(Refusal::CapacityInsufficient {
                    needed: sectors.saturating_mul(admitted.sector),
                    available: (copy
                        .header
                        .last_usable_lba
                        .saturating_sub(copy.header.first_usable_lba)
                        + 1)
                    .saturating_mul(admitted.sector),
                });
            }
            for (number, entry) in admission::gpt_used(copy) {
                if number == partition {
                    continue;
                }
                if admission::overlaps(
                    start,
                    sectors,
                    entry.starting_lba,
                    entry.ending_lba - entry.starting_lba + 1,
                ) {
                    return Err(Refusal::PartitionOverlap { number });
                }
            }
        }
        admission::TableView::Mbr(mbr) => {
            if mbr.extended().is_some() {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR 扩展分区".to_owned(),
                    why: "含扩展分区的目标不参与调整".to_owned(),
                });
            }
            u32::try_from(start).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR 起点超过 32 位".to_owned(),
            })?;
            u32::try_from(sectors).map_err(|_| Refusal::ArgumentOutOfRange {
                what: "MBR 长度超过 32 位".to_owned(),
            })?;
            for (number, entry) in admission::mbr_used(mbr) {
                if number == partition {
                    continue;
                }
                if admission::overlaps(
                    start,
                    sectors,
                    u64::from(entry.start_lba),
                    u64::from(entry.size_lba),
                ) {
                    return Err(Refusal::PartitionOverlap { number });
                }
            }
        }
    }
    let layout = match format {
        TableFormat::Gpt => Some(capacity::gpt_layout(admitted.length, admitted.sector)?),
        TableFormat::Mbr => None,
    };
    let regions = capacity::table_regions(format, layout, admitted.sector)?;
    let steps = vec![
        Step::new(
            StepKind::ResizePartition {
                format,
                partition,
                start_lba: start,
                sectors,
            },
            regions,
        ),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admitted.identity)
}
