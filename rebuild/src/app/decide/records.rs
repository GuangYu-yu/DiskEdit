//! `records`：记录目录的只读视图。
//!
//! 三条子命令都不写盘：`list` 列目标记录目录的条目，`verify` 校验其中标识相符的
//! 一条，`scan` 枚举一个容器下的全部记录目录。真正的读取与判定在只读渲染通道里
//! 完成，这里只检查前置事实。

use diskedit_core::fingerprint::Strength;

use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::Snapshot;

pub fn list() -> Result<Plan, Refusal> {
    Ok(Plan::read_only(vec![Step::new(
        StepKind::ListRecords,
        Vec::new(),
    )]))
}

pub fn verify(snapshot: &Snapshot, token: String) -> Result<Plan, Refusal> {
    let identity = snapshot
        .identity
        .value()
        .copied()
        .filter(|fingerprint| fingerprint.strength() == Strength::Strong);
    if identity.is_none() {
        return Err(Refusal::IdentityUnprovable {
            why: "目标身份读不出，无法定位记录目录".to_owned(),
        });
    }
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
