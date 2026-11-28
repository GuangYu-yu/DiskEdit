//! `replay` 的唯一执行器：补完已搬移作业的表区回放。
//!
//! 与 `undo` 共用同一条回放通路：判定层已经放行了数据区步骤，执行器不区分
//! 两种形态，逆序写回原像并读回核对。

use std::path::Path;

use super::Outcome;
use super::undo;
use crate::app::decide::ConfirmedPlan;
use crate::platform::Platform;

/// 执行一份已确认的表区回放计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    undo::run(platform, target, plan)
}
