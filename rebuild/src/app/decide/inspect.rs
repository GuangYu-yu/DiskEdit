//! `inspect`：逐分区只读明细。

use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};

pub fn plan() -> Result<Plan, Refusal> {
    Ok(Plan::read_only(vec![Step::new(
        StepKind::Inspect,
        Vec::new(),
    )]))
}
