//! 文案：恢复三要素与拒绝的下一步。
//!
//! `5` 的输出必须同时出现"目标可能已改变、证据路径、下一步命令"三样；文案在
//! 这里唯一生成，避免各调用点各写一份。

/// 下一步命令的固定取值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextStep {
    /// 只读检查现状。
    Inspect,
    /// 回滚记录或重放。
    Recover,
    /// 放弃回滚。
    Release,
}

impl NextStep {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Recover => "recover",
            Self::Release => "recover --release",
        }
    }
}

/// 部分完成/待恢复的三要素文案。
pub fn partial(target: &str, evidence: &str, next: NextStep) -> String {
    format!(
        "target may have changed: {target}; evidence: {evidence}; next: diskedit {} {target} --yes",
        next.command()
    )
}
