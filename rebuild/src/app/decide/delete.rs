//! `delete`：清除一个分区条目。

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{PartitionSelector, Snapshot, TableFormat};

pub fn plan(snapshot: &Snapshot, selector: PartitionSelector) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let partition = admission::select(snapshot, selector)?;
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
