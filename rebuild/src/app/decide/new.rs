//! `new`：在空目标上建立分区表。
//!
//! 覆盖既有表需要显式旗标；容量与区间在这里算出，写盘由执行层完成。

use diskedit_core::evidence::{Evidence, Observation};
use diskedit_core::table::GptEvidence;

use super::common::admission;
use super::common::capacity;
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;
use crate::app::observe::TableFormat;

pub fn plan(snapshot: &Snapshot, table: TableFormat, overwrite: bool) -> Result<Plan, Refusal> {
    let admission = admission::admit(snapshot)?;
    if !overwrite && has_table(snapshot) {
        return Err(Refusal::OverwriteNotConfirmed);
    }
    let regions = match table {
        TableFormat::Gpt => {
            let layout = capacity::gpt_layout(admission.length, admission.sector)?;
            capacity::table_regions(table, Some(layout), admission.sector)?
        }
        TableFormat::Mbr => {
            capacity::require_mbr_capacity(admission.length, admission.sector)?;
            capacity::table_regions(table, None, admission.sector)?
        }
    };
    let steps = vec![
        Step::new(StepKind::WriteTable { format: table }, regions),
        Step::new(StepKind::RefreshKernelView, Vec::new()),
    ];
    Plan::write(steps, admission.identity)
}

fn has_table(snapshot: &Snapshot) -> bool {
    matches!(
        &snapshot.table.gpt,
        Observation::Known {
            value: GptEvidence::Usable(_),
            ..
        }
    ) || matches!(
        &snapshot.table.mbr,
        Observation::Known {
            value: Evidence::Present(_),
            ..
        }
    )
}
