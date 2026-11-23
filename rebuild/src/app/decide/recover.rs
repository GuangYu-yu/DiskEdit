//! `recover`：只改记录目录，不动目标字节。
//!
//! 恢复是写操作：需要确认旗标与强身份，但步骤本身不落在目标区间上。三种形态
//! 只改变恢复动作，判定前置条件相同。

use super::common::admission;
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::{RecoverMode, Snapshot};

pub fn plan(snapshot: &Snapshot, mode: RecoverMode) -> Result<Plan, Refusal> {
    let identity = admission::strong_identity(snapshot)?;
    Plan::external(
        vec![Step::new(StepKind::RecoverRecords { mode }, Vec::new())],
        identity,
    )
}
