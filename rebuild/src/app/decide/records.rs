//! `records`：只读记录视图。

use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};

pub fn list() -> Result<Plan, Refusal> {
    Ok(Plan::read_only(vec![Step::new(
        StepKind::ListRecords,
        Vec::new(),
    )]))
}

pub fn verify(token: String) -> Result<Plan, Refusal> {
    Ok(Plan::read_only(vec![Step::new(
        StepKind::VerifyRecord { token },
        Vec::new(),
    )]))
}

pub fn scan() -> Result<Plan, Refusal> {
    Ok(Plan::read_only(vec![Step::new(
        StepKind::ScanRecords,
        Vec::new(),
    )]))
}
