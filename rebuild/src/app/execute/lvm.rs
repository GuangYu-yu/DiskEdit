//! LVM PV 链的执行：经记录意图包围执行 PV resize，必要时再扩 LV。
//!
//! 链改的是目标上的元数据，没有可回滚原像，因此与文件系统步骤同一口径：先落一条
//! 不可逆意图记录，再动字节。镜像经 loop 绑定分区区间，整盘经分区节点寻址。

use std::path::Path;

use diskedit_core::region::Region;

use super::transaction::RecordSession;
use super::{ExecError, FailureKind, Outcome};
use crate::app::decide::{ConfirmedPlan, Step, StepKind};
use crate::app::lvm::{self, GrowChain, LvmError};
use crate::platform::Platform;

/// 意图记录里的固定标签：一次 PV 元数据改写。
const LVM_INTENT: &[u8] = b"lvm-pv";

/// 执行一份只含 LVM 步骤的计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    match execute_plan(platform, target, plan) {
        Ok(()) => Outcome::completed(),
        Err((error, started)) => {
            if started {
                Outcome::needs_recovery(error.to_string())
            } else {
                Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string())
            }
        }
    }
}

enum Action {
    Shrink {
        partition: u32,
        bytes: u64,
    },
    Grow {
        partition: u32,
        old_bytes: u64,
        new_bytes: u64,
        grow_lv: bool,
        lv: Option<String>,
    },
}

fn action(steps: &[Step]) -> Result<(Action, Region), ExecError> {
    for step in steps {
        let action = match &step.kind {
            StepKind::LvmPvShrink {
                partition, bytes, ..
            } => Some(Action::Shrink {
                partition: *partition,
                bytes: *bytes,
            }),
            StepKind::LvmGrowChain {
                partition,
                old_bytes,
                new_bytes,
                grow_lv,
                lv,
                ..
            } => Some(Action::Grow {
                partition: *partition,
                old_bytes: *old_bytes,
                new_bytes: *new_bytes,
                grow_lv: *grow_lv,
                lv: lv.clone(),
            }),
            StepKind::Inspect
            | StepKind::Check { .. }
            | StepKind::ListRecords
            | StepKind::VerifyRecord { .. }
            | StepKind::ScanRecords
            | StepKind::WriteTable { .. }
            | StepKind::AddPartition { .. }
            | StepKind::DeletePartition { .. }
            | StepKind::ResizePartition { .. }
            | StepKind::SetProperty { .. }
            | StepKind::MakeFilesystem { .. }
            | StepKind::GrowFilesystem { .. }
            | StepKind::ShrinkFilesystem { .. }
            | StepKind::RebuildSwap { .. }
            | StepKind::GrowFilesystemOnline { .. }
            | StepKind::ShrinkFilesystemOnline { .. }
            | StepKind::CopyPartition { .. }
            | StepKind::MovePartition { .. }
            | StepKind::RefreshKernelView { .. }
            | StepKind::RecoverRecords { .. } => None,
        };
        if let Some(action) = action {
            let region = step.regions.first().copied().ok_or_else(|| {
                ExecError::InvalidPlan("the LVM step declares no region".to_owned())
            })?;
            return Ok((action, region));
        }
    }
    Err(ExecError::InvalidPlan(
        "the plan has no LVM step".to_owned(),
    ))
}

pub(super) fn execute_plan(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<(), (ExecError, bool)> {
    let (action, region) = action(plan.steps()).map_err(|error| (error, false))?;
    super::filesystem::ensure_block_capability(platform, target).map_err(|error| (error, false))?;
    super::filesystem::check_identity(platform, target, plan).map_err(|error| (error, false))?;
    let mut session =
        RecordSession::prepare(platform, plan.identity()).map_err(|error| (error, false))?;
    super::filesystem::check_identity(platform, target, plan).map_err(|error| (error, false))?;
    session
        .begin_irreversible(plan.request_digest(), LVM_INTENT)
        .map_err(|error| (error, false))?;

    let partition = match &action {
        Action::Shrink { partition, .. } | Action::Grow { partition, .. } => *partition,
    };
    let device = super::filesystem::resolve_device(
        platform,
        target,
        partition,
        region.start(),
        region.len(),
    )
    .map_err(|error| (error, false))?;

    let result = match &action {
        Action::Shrink { bytes, .. } => lvm::shrink_pv(platform, device.device(), *bytes),
        Action::Grow {
            old_bytes,
            new_bytes,
            grow_lv,
            lv,
            ..
        } => lvm::grow_chain(
            platform,
            &GrowChain {
                part_dev: device.device(),
                expected_bytes: *new_bytes,
                delta_bytes: new_bytes.saturating_sub(*old_bytes),
                grow_lv: *grow_lv,
                want_lv: lv.as_deref(),
            },
        ),
    };
    if let Err(error) = result {
        return Err((lvm_exec_error(error), true));
    }
    session.commit().map_err(|error| (error, true))?;
    Ok(())
}

fn lvm_exec_error(error: LvmError) -> ExecError {
    ExecError::Io(error.to_string())
}
