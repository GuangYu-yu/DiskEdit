//! `copy`：把一个分区的内容复制到另一个分区。
//!
//! 只有 GPT 目标参与；目标分区必须放得下来源分区的内容。

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;
use crate::app::observe::TableFormat;

pub fn plan(snapshot: &Snapshot, source: u32, target: u32) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    if source == target {
        return Err(Refusal::ArgumentOutOfRange {
            what: "来源与目标是同一个分区".to_owned(),
        });
    }
    let table = admission::table(snapshot)?;
    if table.format() == TableFormat::Mbr {
        return Err(Refusal::ContentUnsupported {
            what: "MBR 分区复制".to_owned(),
            why: "复制只对 GPT 目标开放".to_owned(),
        });
    }
    let source_facts = admission::partition(snapshot, source)?;
    let target_facts = admission::partition(snapshot, target)?;
    if target_facts.partition.sectors < source_facts.partition.sectors {
        return Err(Refusal::CapacityInsufficient {
            needed: source_facts
                .partition
                .sectors
                .saturating_mul(admitted.sector),
            available: target_facts
                .partition
                .sectors
                .saturating_mul(admitted.sector),
        });
    }
    let region = capacity::partition_region(
        target_facts.partition.start_lba,
        target_facts.partition.sectors,
        admitted.sector,
    )?;
    let steps = vec![Step::new(
        StepKind::CopyPartition { source, target },
        vec![region],
    )];
    Plan::write(steps, admitted.identity)
}
