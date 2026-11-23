//! `move`：平移一个分区。
//!
//! 新区间必须落在可用范围内且不与其它分区重叠；计划声明旧区间与新区间两块写区。

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;

pub fn plan(snapshot: &Snapshot, partition: u32, offset_sectors: i64) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    if offset_sectors == 0 {
        return Err(Refusal::ArgumentOutOfRange {
            what: "位移为零".to_owned(),
        });
    }
    let facts = admission::partition(snapshot, partition)?;
    let table = admission::table(snapshot)?;
    let copy = match table {
        admission::TableView::Gpt(copy) => copy,
        admission::TableView::Mbr(_) => {
            return Err(Refusal::ContentUnsupported {
                what: "MBR 分区平移".to_owned(),
                why: "平移只对 GPT 目标开放".to_owned(),
            });
        }
    };
    let new_start = if offset_sectors >= 0 {
        facts.partition.start_lba.checked_add(offset_sectors as u64)
    } else {
        facts
            .partition
            .start_lba
            .checked_sub(offset_sectors.unsigned_abs())
    }
    .ok_or_else(|| Refusal::ArgumentOutOfRange {
        what: "平移后的起点越过可寻址范围".to_owned(),
    })?;
    let end_lba = new_start
        .checked_add(facts.partition.sectors - 1)
        .ok_or_else(|| Refusal::ArgumentOutOfRange {
            what: "平移后的终点越过可寻址上界".to_owned(),
        })?;
    if new_start < copy.header.first_usable_lba || end_lba > copy.header.last_usable_lba {
        return Err(Refusal::CapacityInsufficient {
            needed: facts.partition.sectors.saturating_mul(admitted.sector),
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
            new_start,
            facts.partition.sectors,
            entry.starting_lba,
            entry.ending_lba - entry.starting_lba + 1,
        ) {
            return Err(Refusal::PartitionOverlap { number });
        }
    }
    let old = capacity::partition_region(
        facts.partition.start_lba,
        facts.partition.sectors,
        admitted.sector,
    )?;
    let new = capacity::partition_region(new_start, facts.partition.sectors, admitted.sector)?;
    let steps = vec![
        Step::new(
            StepKind::MovePartition {
                partition,
                offset_sectors,
            },
            vec![old, new],
        ),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admitted.identity)
}
