//! 执行层的基础类型：状态机、写入令牌与唯一写通道。
//!
//! 执行只接收已确认的计划与独占句柄；写入必须先有记录原像，再经唯一写通道落到
//! 声明区间内。具体步骤的执行器按纵切逐个接入，这里只固定所有写路径共用的
//! 约束。

pub mod diagnose;
pub mod digest;
pub mod filesystem;
pub mod postcondition;
pub mod recover;
pub mod relocate;
pub mod resize;
pub mod state;
pub mod table;
pub mod token;
pub mod transaction;
pub mod write;

pub use state::{ExecutionState, StateMachine, TransitionError};
pub use token::Recorded;
pub use write::WriteChannel;

use crate::platform::PlatformError;

/// 执行期错误；报告层按变体给出退出码。
#[derive(Debug)]
pub enum ExecError {
    /// 平台端口报错。
    Platform(PlatformError),
    /// 记录不可用或不匹配。
    Record(String),
    /// 目标身份在持锁后发生变化。
    IdentityChanged(String),
    /// 本平台不提供所需能力。
    Unsupported(String),
    /// 计划内部不自洽，例如区间未被声明。
    InvalidPlan(String),
    /// 目标读写失败。
    Io(String),
    /// 回读结果与写入不一致。
    Postcondition(String),
    /// 故障注入：模拟写入中途被打断。
    Injected(String),
}

impl From<PlatformError> for ExecError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Platform(error) => write!(f, "{error}"),
            Self::Record(why)
            | Self::IdentityChanged(why)
            | Self::Unsupported(why)
            | Self::InvalidPlan(why)
            | Self::Io(why)
            | Self::Postcondition(why)
            | Self::Injected(why) => f.write_str(why),
        }
    }
}

/// 未完成时的失败类别；报告层据此选退出码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// 拒绝：身份、记录或前置条件不成立。
    Refused,
    /// 本机不支持所需能力。
    Unsupported,
    /// 已进入执行但目标字节未改变的环境失败。
    FailedBeforeWrite,
    /// 写了目标字节而事务未收尾。
    NeedsRecovery,
    /// 内部错误。
    Internal,
}

/// 一次执行的结果。
///
/// 字段私有且只能经状态机方法产出，调用方无法拼出一个"看起来已提交"的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    state: ExecutionState,
    failure: Option<FailureKind>,
    reason: Option<String>,
}

impl Outcome {
    pub(crate) fn new(
        state: ExecutionState,
        failure: Option<FailureKind>,
        reason: Option<String>,
    ) -> Self {
        Self {
            state,
            failure,
            reason,
        }
    }

    /// 完成且无保留。
    pub fn completed() -> Self {
        Self::new(ExecutionState::Committed, None, None)
    }

    /// 未写目标字节的失败。
    pub fn unwritten(failure: FailureKind, reason: impl Into<String>) -> Self {
        Self::new(
            ExecutionState::Unwritten,
            Some(failure),
            Some(reason.into()),
        )
    }

    /// 已写目标字节而事务未收尾。
    pub fn needs_recovery(reason: impl Into<String>) -> Self {
        Self::new(
            ExecutionState::NeedsRecovery,
            Some(FailureKind::NeedsRecovery),
            Some(reason.into()),
        )
    }

    /// 写入完成但内核视图未同步。
    pub fn committed_unsynced(reason: impl Into<String>) -> Self {
        Self::new(
            ExecutionState::CommittedUnsynced,
            Some(FailureKind::NeedsRecovery),
            Some(reason.into()),
        )
    }

    /// 目标写入已按记录原像回滚。
    pub fn rolled_back() -> Self {
        Self::new(ExecutionState::RolledBack, None, None)
    }

    pub fn state(&self) -> ExecutionState {
        self.state
    }

    pub fn failure(&self) -> Option<FailureKind> {
        self.failure
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn succeeded(&self) -> bool {
        self.state == ExecutionState::Committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwritten_outcomes_carry_their_failure() {
        let outcome = Outcome::unwritten(FailureKind::Refused, "no");
        assert_eq!(outcome.state(), ExecutionState::Unwritten);
        assert_eq!(outcome.failure(), Some(FailureKind::Refused));
        assert_eq!(outcome.reason(), Some("no"));
        assert!(!outcome.succeeded());
    }

    // covers: D1
    #[test]
    fn recovery_outcomes_are_not_success() {
        let outcome = Outcome::needs_recovery("interrupted");
        assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
        assert!(!outcome.succeeded());
    }

    // covers: D1
    #[test]
    fn completed_is_the_only_success() {
        assert!(Outcome::completed().succeeded());
        assert!(!Outcome::committed_unsynced("unsynced").succeeded());
    }
}
