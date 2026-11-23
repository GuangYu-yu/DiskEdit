//! 执行状态机：写前、已登记、已提交与待恢复。
//!
//! 状态只能由 [`StateMachine`] 的方法推进；字段不公开，因此调用方无法直接构造
//! 一个"看起来已提交"的状态。终态决定报告层给出的退出码。

/// 一次执行所处的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionState {
    /// 尚未写目标字节，也没有登记原像。
    Unwritten,
    /// 原像已落盘，可以写目标字节。
    Armed,
    /// 目标写入完成、记录已提交、内核视图已同步。
    Committed,
    /// 目标写入完成、记录已提交，但内核视图未同步。
    CommittedUnsynced,
    /// 目标写入已回滚。
    RolledBack,
    /// 写入已开始而未完成，需要恢复。
    NeedsRecovery,
}

impl ExecutionState {
    /// 是否已经写过目标字节。
    pub const fn touched_target(self) -> bool {
        matches!(
            self,
            Self::Armed | Self::CommittedUnsynced | Self::NeedsRecovery
        )
    }

    /// 是否需要一个确定的恢复动作。
    pub const fn needs_recovery(self) -> bool {
        matches!(self, Self::Armed | Self::NeedsRecovery)
    }
}

/// 非法状态推进。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub from: ExecutionState,
    pub wanted: &'static str,
}

/// 状态机；初始状态恒为 [`ExecutionState::Unwritten`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateMachine {
    state: ExecutionState,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    pub const fn new() -> Self {
        Self {
            state: ExecutionState::Unwritten,
        }
    }

    pub const fn state(&self) -> ExecutionState {
        self.state
    }

    /// 原像落盘后进入已登记；只允许从写前推进。
    pub fn arm(&mut self) -> Result<(), TransitionError> {
        self.transition(ExecutionState::Unwritten, ExecutionState::Armed, "arm")
    }

    /// 记录终态已落盘且内核视图已同步。
    pub fn commit(&mut self) -> Result<(), TransitionError> {
        self.transition(ExecutionState::Armed, ExecutionState::Committed, "commit")
    }

    /// 记录终态已落盘但内核视图未同步。
    pub fn commit_unsynced(&mut self) -> Result<(), TransitionError> {
        self.transition(
            ExecutionState::Armed,
            ExecutionState::CommittedUnsynced,
            "commit-unsynced",
        )
    }

    /// 目标写入失败且未回滚。
    pub fn needs_recovery(&mut self) -> Result<(), TransitionError> {
        self.transition(
            ExecutionState::Armed,
            ExecutionState::NeedsRecovery,
            "needs-recovery",
        )
    }

    /// 回滚完成。
    pub fn roll_back(&mut self) -> Result<(), TransitionError> {
        self.transition(
            ExecutionState::Armed,
            ExecutionState::RolledBack,
            "roll-back",
        )
    }

    fn transition(
        &mut self,
        from: ExecutionState,
        to: ExecutionState,
        wanted: &'static str,
    ) -> Result<(), TransitionError> {
        if self.state != from {
            return Err(TransitionError {
                from: self.state,
                wanted,
            });
        }
        self.state = to;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_unwritten_and_arms_once() {
        let mut machine = StateMachine::new();
        assert_eq!(machine.state(), ExecutionState::Unwritten);
        machine.arm().expect("首次登记应成立");
        assert_eq!(machine.state(), ExecutionState::Armed);
        assert_eq!(
            machine.arm(),
            Err(TransitionError {
                from: ExecutionState::Armed,
                wanted: "arm"
            })
        );
    }

    #[test]
    fn commit_and_unsynced_are_terminal() {
        let mut committed = StateMachine::new();
        committed.arm().expect("登记应成立");
        committed.commit().expect("提交应成立");
        assert_eq!(committed.state(), ExecutionState::Committed);
        assert!(!committed.state().needs_recovery());

        let mut unsynced = StateMachine::new();
        unsynced.arm().expect("登记应成立");
        unsynced.commit_unsynced().expect("未同步提交应成立");
        assert_eq!(unsynced.state(), ExecutionState::CommittedUnsynced);
        assert!(unsynced.state().touched_target());
    }

    #[test]
    fn arm_is_required_before_any_later_state() {
        let mut machine = StateMachine::new();
        assert!(machine.commit().is_err());
        assert!(machine.needs_recovery().is_err());
        assert!(machine.roll_back().is_err());
        assert_eq!(machine.state(), ExecutionState::Unwritten);
    }
}
