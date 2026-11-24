//! 执行：把已确认的计划变成目标字节，并把过程写进持久记录。
//!
//! 执行只接收 [`ConfirmedPlan`] 与独占句柄；写入必须先有记录原像，再经唯一写
//! 通道落到声明区间内，最后回读核对。失败落在哪一状态由 [`ExecutionState`]
//! 决定，报告层只读终态，不猜过程。
//!
//! [`ConfirmedPlan`]: crate::app::decide::ConfirmedPlan

pub mod check;
pub mod digest;
pub mod lvm;
pub mod online;
pub mod postcondition;
pub mod records;
pub mod recover;
pub mod relocate;
pub mod state;
pub mod steps;
pub mod token;
pub mod transaction;
pub mod write;

use std::path::Path;

use crate::app::decide::ConfirmedPlan;
use crate::platform::{Platform, PlatformError};

pub use state::ExecutionState;
pub use token::Recorded;
pub use write::WriteChannel;

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
    /// 一致性检查器报告问题；这是检查结论，不是本实现的失败。
    Check(String),
    /// 故障注入；仅测试使用。
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
            | Self::Check(why)
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
    pub(crate) fn completed() -> Self {
        Self::new(ExecutionState::Committed, None, None)
    }

    /// 未写目标字节的失败。
    pub(crate) fn unwritten(failure: FailureKind, reason: impl Into<String>) -> Self {
        Self::new(
            ExecutionState::Unwritten,
            Some(failure),
            Some(reason.into()),
        )
    }

    /// 已写目标字节而事务未收尾。
    pub(crate) fn needs_recovery(reason: impl Into<String>) -> Self {
        Self::new(
            ExecutionState::NeedsRecovery,
            Some(FailureKind::NeedsRecovery),
            Some(reason.into()),
        )
    }

    /// 写入完成但内核视图未同步。
    pub(crate) fn committed_unsynced(reason: impl Into<String>) -> Self {
        Self::new(
            ExecutionState::CommittedUnsynced,
            Some(FailureKind::NeedsRecovery),
            Some(reason.into()),
        )
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

/// 故障注入点；生产路径全部为 `false`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// 写目标字节之前中断。
    pub before_write: bool,
    /// 写入过程中中断。
    pub mid_write: bool,
    /// 全部写入完成后、提交记录之前中断。
    pub after_write: bool,
}

impl Faults {
    pub const NONE: Self = Self {
        before_write: false,
        mid_write: false,
        after_write: false,
    };
}

/// 执行入口。
pub fn execute(
    confirmed: &ConfirmedPlan,
    platform: &dyn Platform,
    target: &Path,
    generation: u64,
) -> Outcome {
    transaction::run(confirmed, platform, target, generation, Faults::NONE)
}

/// 带故障注入的执行入口；仅测试与仪器使用。
pub fn execute_with_faults(
    confirmed: &ConfirmedPlan,
    platform: &dyn Platform,
    target: &Path,
    generation: u64,
    faults: Faults,
) -> Outcome {
    transaction::run(confirmed, platform, target, generation, faults)
}
