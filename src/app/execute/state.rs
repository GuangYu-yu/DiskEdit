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

    /// 原像落盘后进入已登记。
    ///
    /// 登记的是"这次执行已经开始写"，而不是单个步骤；同一份计划里的后续写步骤
    /// 落各自原像时仍是已登记，不重复推进。已提交或待恢复之后不能再登记。
    pub fn arm(&mut self) -> Result<(), TransitionError> {
        match self.state {
            ExecutionState::Unwritten => {
                self.state = ExecutionState::Armed;
                Ok(())
            }
            ExecutionState::Armed => Ok(()),
            other @ (ExecutionState::Committed
            | ExecutionState::CommittedUnsynced
            | ExecutionState::RolledBack
            | ExecutionState::NeedsRecovery) => Err(TransitionError {
                from: other,
                wanted: "arm",
            }),
        }
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
    fn arming_is_idempotent_within_one_execution() {
        let mut machine = StateMachine::new();
        assert_eq!(machine.state(), ExecutionState::Unwritten);
        machine.arm().expect("首次登记应成立");
        assert_eq!(machine.state(), ExecutionState::Armed);
        // 同一份计划里的第二个写步骤会再次登记原像；已登记不重复推进。
        machine.arm().expect("后续步骤登记应成立");
        assert_eq!(machine.state(), ExecutionState::Armed);
        machine.commit().expect("多步之后仍能提交");
        assert_eq!(machine.state(), ExecutionState::Committed);
        assert_eq!(
            machine.arm(),
            Err(TransitionError {
                from: ExecutionState::Committed,
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

    #[test]
    fn every_state_derives_its_recovery_answer_from_one_table() {
        // 报告与恢复都只问状态：这两个结论对每个状态只有一个答案，不是各处自行判断。
        for (state, recovery, touched) in [
            (ExecutionState::Unwritten, false, false),
            (ExecutionState::Armed, true, true),
            (ExecutionState::Committed, false, false),
            (ExecutionState::CommittedUnsynced, false, true),
            (ExecutionState::RolledBack, false, false),
            (ExecutionState::NeedsRecovery, true, true),
        ] {
            assert_eq!(state.needs_recovery(), recovery, "needs_recovery {state:?}");
            assert_eq!(state.touched_target(), touched, "touched_target {state:?}");
        }
    }

    #[test]
    fn the_only_way_out_of_unwritten_is_arming() {
        for attempt in 0..3 {
            let mut machine = StateMachine::new();
            let refused = match attempt {
                0 => machine.commit().is_err(),
                1 => machine.commit_unsynced().is_err(),
                _ => machine.roll_back().is_err(),
            };
            assert!(refused, "未登记原像前不得离开未写态");
            assert_eq!(machine.state(), ExecutionState::Unwritten);
        }
        // 已登记之后，四条推进各自成立，且都不回到未写态。
        for (index, next) in [
            ExecutionState::Committed,
            ExecutionState::CommittedUnsynced,
            ExecutionState::NeedsRecovery,
            ExecutionState::RolledBack,
        ]
        .into_iter()
        .enumerate()
        {
            let mut machine = StateMachine::new();
            machine.arm().expect("登记应成立");
            match index {
                0 => machine.commit(),
                1 => machine.commit_unsynced(),
                2 => machine.needs_recovery(),
                _ => machine.roll_back(),
            }
            .expect("登记后的推进应成立");
            assert_eq!(machine.state(), next);
            assert_ne!(machine.state(), ExecutionState::Unwritten);
        }
    }
}
