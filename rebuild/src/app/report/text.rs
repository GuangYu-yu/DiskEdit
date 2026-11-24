//! 人读文本：执行结果与拒绝各一条。
//!
//! 文本不作契约；退出码由 [`super::exit`] 产生，这里只负责把同一结论讲清楚。

use crate::app::decide::Refusal;
use crate::app::execute::Outcome;

use super::wording::{self, NextStep};

/// 执行结果的文本；待恢复状态带三要素。
pub fn outcome(outcome: &Outcome, target: &str, evidence: &str) -> String {
    match outcome.state() {
        crate::app::execute::ExecutionState::Committed => "done".to_owned(),
        crate::app::execute::ExecutionState::RolledBack => "rolled back".to_owned(),
        crate::app::execute::ExecutionState::CommittedUnsynced => explained(
            wording::partial(target, evidence, NextStep::Inspect),
            outcome,
        ),
        crate::app::execute::ExecutionState::NeedsRecovery => explained(
            wording::partial(target, evidence, NextStep::Recover),
            outcome,
        ),
        crate::app::execute::ExecutionState::Armed => explained(
            wording::partial(target, evidence, NextStep::Recover),
            outcome,
        ),
        crate::app::execute::ExecutionState::Unwritten => outcome
            .reason()
            .map_or_else(|| "not executed".to_owned(), str::to_owned),
    }
}

/// 在三要素之后补上具体原因；三要素本身不变。
fn explained(base: String, outcome: &Outcome) -> String {
    match outcome.reason() {
        Some(reason) => format!("{base}; cause: {reason}"),
        None => base,
    }
}

/// 拒绝的文本；下一步固定来自 [`Refusal::next_step`]。
pub fn refusal(refusal: &Refusal) -> String {
    format!("request refused; next: {}", refusal.next_step())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_text_keeps_the_three_elements_and_adds_the_cause() {
        let partial = Outcome::needs_recovery("载体步骤失败（类别 2）：权限不足");
        let text = outcome(&partial, "target.img", "target.img");
        assert!(text.contains("target may have changed: target.img"));
        assert!(text.contains("evidence: target.img"));
        assert!(text.contains("next: diskedit recover target.img --yes"));
        assert!(text.ends_with("; cause: 载体步骤失败（类别 2）：权限不足"));
    }

    #[test]
    fn committed_text_stays_bare() {
        let done = Outcome::completed();
        assert_eq!(outcome(&done, "target.img", "target.img"), "done");
    }
}
