//! 人读文本：执行结果与拒绝各一条。
//!
//! 文本不作契约；退出码由 [`super::exit`] 产生，这里只负责把同一结论讲清楚。

use crate::app::decide::Refusal;
use crate::app::execute::Outcome;

use super::wording::{self, NextStep};

/// 执行结果的文本；待恢复状态带三要素。
pub fn outcome(outcome: &Outcome, target: &str, evidence: &str) -> String {
    match outcome.state() {
        crate::app::execute::ExecutionState::Committed => "完成".to_owned(),
        crate::app::execute::ExecutionState::RolledBack => "已回滚".to_owned(),
        crate::app::execute::ExecutionState::CommittedUnsynced => {
            wording::partial(target, evidence, NextStep::Inspect)
        }
        crate::app::execute::ExecutionState::NeedsRecovery => {
            wording::partial(target, evidence, NextStep::Recover)
        }
        crate::app::execute::ExecutionState::Armed => {
            wording::partial(target, evidence, NextStep::Recover)
        }
        crate::app::execute::ExecutionState::Unwritten => outcome
            .reason()
            .map_or_else(|| "未执行".to_owned(), str::to_owned),
    }
}

/// 拒绝的文本；下一步固定来自 [`Refusal::next_step`]。
pub fn refusal(refusal: &Refusal) -> String {
    format!("请求被拒绝；下一步：{}", refusal.next_step())
}
