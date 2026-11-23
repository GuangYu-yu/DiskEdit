//! `records`：记录目录的只读视图。
//!
//! 列出条目只读记录目录；按标识校验与按目录扫描需要额外的记录解析端口，当前
//! 平台端口尚未提供，明确拒绝而不是返回空结果。

use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};

pub fn list() -> Result<Plan, Refusal> {
    Ok(Plan::read_only(vec![Step::new(
        StepKind::ListRecords,
        Vec::new(),
    )]))
}

pub fn verify(_token: String) -> Result<Plan, Refusal> {
    Err(Refusal::ContentUnsupported {
        what: "记录校验".to_owned(),
        why: "按标识读取记录的端口尚未接入".to_owned(),
    })
}

pub fn scan() -> Result<Plan, Refusal> {
    Err(Refusal::ContentUnsupported {
        what: "记录目录扫描".to_owned(),
        why: "递归扫描记录目录的端口尚未接入".to_owned(),
    })
}
