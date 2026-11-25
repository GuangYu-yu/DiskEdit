//! 退出码的唯一映射。
//!
//! 七个值来自行为基线第 2 节；判定顺序固定，命中即停。执行终态与拒绝成因各
//! 自映射到同一组常量，除此之外没有第二处产码。

use crate::app::decide::Refusal;
use crate::app::execute::{ExecutionState, FailureKind, Outcome};

/// 成功：未写目标且未建记录，或记录已清理且视图已同步，或完整回滚并归档。
pub const SUCCESS: u8 = 0;
/// 写目标字节前的不变量破坏或内部错误。
pub const INTERNAL: u8 = 1;
/// 用法与解析错误。
pub const USAGE: u8 = 2;
/// 请求被拒、记录损坏、证据指向外来或损坏。
pub const REFUSED: u8 = 3;
/// 已进入执行、目标字节未变的 I/O 或环境故障。
pub const ENVIRONMENT: u8 = 4;
/// 部分完成、待恢复，或持久状态已达成但内核视图未同步。
pub const PARTIAL: u8 = 5;
/// 能力不支持。
pub const UNSUPPORTED: u8 = 6;

/// 一次执行结果对应的退出码。
pub fn exit_of(outcome: &Outcome) -> u8 {
    match outcome.state() {
        ExecutionState::Committed | ExecutionState::RolledBack => SUCCESS,
        ExecutionState::CommittedUnsynced | ExecutionState::NeedsRecovery => PARTIAL,
        ExecutionState::Armed => PARTIAL,
        ExecutionState::Unwritten => match outcome.failure() {
            Some(FailureKind::Refused) => REFUSED,
            Some(FailureKind::Unsupported) => UNSUPPORTED,
            Some(FailureKind::FailedBeforeWrite) => ENVIRONMENT,
            Some(FailureKind::NeedsRecovery) => PARTIAL,
            Some(FailureKind::Internal) | None => INTERNAL,
        },
    }
}

/// 一次拒绝对应的退出码。
pub fn exit_of_refusal(refusal: &Refusal) -> u8 {
    match refusal {
        Refusal::ConfirmationRequired
        | Refusal::OverwriteNotConfirmed
        | Refusal::ArgumentOutOfRange { .. } => USAGE,
        Refusal::PlanDigestMismatch => INTERNAL,
        Refusal::IdentityUnprovable { .. }
        | Refusal::GeometryUnknown { .. }
        | Refusal::ContentUnsupported { .. }
        | Refusal::ToolMissing { .. }
        | Refusal::PlatformCannotRunTools { .. }
        | Refusal::MountNotBlockDevice { .. }
        | Refusal::MountUnmapped { .. } => UNSUPPORTED,
        Refusal::IdentityChanged
        | Refusal::OccupancyUnknown { .. }
        | Refusal::Occupied { .. }
        | Refusal::FactUnknown { .. }
        | Refusal::TableMissing
        | Refusal::TableDamaged { .. }
        | Refusal::GptRequired { .. }
        | Refusal::RecordNotFound { .. }
        | Refusal::RecordStoreMissing { .. }
        | Refusal::RecordDirectoryUnresolved { .. }
        | Refusal::MountNotMounted { .. }
        | Refusal::MountAmbiguous { .. }
        | Refusal::MountNotPartition { .. }
        | Refusal::OnlineShrink { .. }
        | Refusal::PartitionMissing { .. }
        | Refusal::PartitionOverlap { .. }
        | Refusal::CapacityInsufficient { .. }
        | Refusal::FilesystemUnknown { .. }
        | Refusal::MinimumSizeUnknown { .. }
        | Refusal::BelowMinimumSize { .. }
        | Refusal::FlagUnknown { .. }
        | Refusal::PartitionNodeTarget { .. }
        | Refusal::TargetTooSmall { .. }
        | Refusal::TableOccupied { .. }
        | Refusal::PmbrInconsistent { .. }
        | Refusal::RecordUnreadable { .. }
        | Refusal::RecordConflict { .. } => REFUSED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unwritten_failure_has_one_code() {
        for (failure, expected) in [
            (FailureKind::Refused, REFUSED),
            (FailureKind::Unsupported, UNSUPPORTED),
            (FailureKind::FailedBeforeWrite, ENVIRONMENT),
            (FailureKind::NeedsRecovery, PARTIAL),
            (FailureKind::Internal, INTERNAL),
        ] {
            let outcome = Outcome::new(ExecutionState::Unwritten, Some(failure), None);
            assert_eq!(exit_of(&outcome), expected, "{failure:?}");
        }
    }

    #[test]
    fn success_and_partial_states_are_fixed() {
        for (state, expected) in [
            (ExecutionState::Committed, SUCCESS),
            (ExecutionState::RolledBack, SUCCESS),
            (ExecutionState::CommittedUnsynced, PARTIAL),
            (ExecutionState::NeedsRecovery, PARTIAL),
        ] {
            let outcome = Outcome::new(state, None, None);
            assert_eq!(exit_of(&outcome), expected, "{state:?}");
        }
    }

    #[test]
    fn refusal_codes_follow_the_baseline_classes() {
        assert_eq!(exit_of_refusal(&Refusal::ConfirmationRequired), USAGE);
        assert_eq!(exit_of_refusal(&Refusal::PlanDigestMismatch), INTERNAL);
        assert_eq!(
            exit_of_refusal(&Refusal::ToolMissing {
                program: diskedit_core::fs::Program::E2fsck
            }),
            UNSUPPORTED
        );
        assert_eq!(
            exit_of_refusal(&Refusal::PartitionMissing { number: 3 }),
            REFUSED
        );
    }
}
