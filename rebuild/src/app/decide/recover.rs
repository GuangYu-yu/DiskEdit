//! `recover`：只改记录目录，不动目标字节。

use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};

pub fn plan() -> Result<Plan, Refusal> {
    Ok(Plan::external(vec![Step::new(
        StepKind::RecoverRecords,
        Vec::new(),
    )]))
}
