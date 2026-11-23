//! `delete`：清除一个分区条目。

use super::common::admission;
use super::common::capacity;
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;
use crate::app::observe::TableFormat;

pub fn plan(snapshot: &Snapshot, partition: u32) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let table = admission::table(snapshot)?;
    let format = table.format();
    let exists = match table {
        admission::TableView::Gpt(copy) => admission::gpt_used(copy).any(|(n, _)| n == partition),
        admission::TableView::Mbr(mbr) => admission::mbr_used(mbr).any(|(n, _)| n == partition),
    };
    if !exists {
        return Err(Refusal::PartitionMissing { number: partition });
    }
    let layout = match format {
        TableFormat::Gpt => Some(capacity::gpt_layout(admitted.length, admitted.sector)?),
        TableFormat::Mbr => None,
    };
    let regions = capacity::table_regions(format, layout, admitted.sector)?;
    let steps = vec![
        Step::new(StepKind::DeletePartition { format, partition }, regions),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admitted.identity)
}
