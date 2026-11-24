//! 判定层：把请求与事实快照变成只读报告、可执行计划或一条拒绝。
//!
//! 判定只做纯计算，不读盘、不运行外部程序。计划在这里成形并被摘要绑定，
//! 执行层只接受已确认的计划值。

pub mod confirmed;
pub mod digest;
pub mod plan;
pub mod refusal;
pub mod step;

pub use confirmed::{ConfirmedPlan, confirm};
pub use plan::Plan;
pub use refusal::Refusal;
pub use step::{Access, Step, StepKind};

/// 一次判定的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// 只读命令的报告计划。
    Report(Plan),
    /// 会改变目标或外部状态的计划，执行前需确认。
    Execute(Plan),
    /// 拒绝，并带上固定成因。
    Refuse(Refusal),
}
