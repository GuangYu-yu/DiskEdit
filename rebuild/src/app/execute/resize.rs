//! `resize` 的执行：把一次扩缩拆成两个相邻事务 —— 文件系统阶段与分区表阶段。
//!
//! 扩容先写表、后扩文件系统；收缩先缩文件系统、后写表（`B13`）。每个阶段各自
//! 走既有的写事务：表阶段登记原像、可回滚；文件系统阶段落一条不可回滚意图。
//! 两阶段之间中断时，盘面仍是自洽的（文件系统不大于分区），可由重跑收敛。

use std::path::Path;

use super::{ExecError, FailureKind, Outcome};
use crate::app::decide::{ConfirmedPlan, StepKind};
use crate::platform::Platform;

/// 执行一份含文件系统步骤与表写步骤的 `resize` 计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    match execute(platform, target, plan) {
        Ok(Some(why)) => Outcome::committed_unsynced(why),
        Ok(None) => Outcome::completed(),
        Err((error, touched)) => failure(error, touched),
    }
}

/// 执行一份已挂载分区的在线 `resize` 计划：写表 → 内核视图 → 扩文件系统。
///
/// 写表之前复核挂载与整盘身份；表提交后用 `BLKPG_RESIZE_PARTITION` 按分区同步内核
/// 视图（整盘重读在分区被占用时返回 `EBUSY`）；最后在用户已有的挂载上扩文件系统。
pub fn run_online(platform: &dyn Platform, plan: &ConfirmedPlan) -> Outcome {
    match execute_online(platform, plan) {
        Ok(None) => Outcome::completed(),
        Ok(Some(why)) => Outcome::committed_unsynced(why),
        Err((error, touched)) => failure(error, touched),
    }
}

fn execute_online(
    platform: &dyn Platform,
    plan: &ConfirmedPlan,
) -> Result<Option<String>, (ExecError, bool)> {
    let refresh = plan
        .steps()
        .iter()
        .find(|step| matches!(step.kind, StepKind::RefreshKernelView { .. }))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the online plan has no kernel view step".to_owned()),
                false,
            )
        })?;
    let StepKind::RefreshKernelView {
        disk,
        partition,
        start_bytes,
        bytes,
    } = &refresh.kind
    else {
        unreachable!("按 RefreshKernelView 选出");
    };
    let (disk, partition, start_bytes, bytes) = (disk.clone(), *partition, *start_bytes, *bytes);
    // 写表之前复核：挂载点仍指向同一分区，整盘身份未变。
    super::filesystem::verify_online(platform, plan).map_err(|error| (error, false))?;
    // 表阶段：提交记录，但不重读整盘（在线路径随后按分区增量同步）。
    super::table::execute_plan(platform, &disk, plan, false)?;
    if let Err(error) = platform.resize_partition_live(&disk, partition, start_bytes, bytes) {
        return Ok(Some(format!(
            "partition table written but kernel sync failed: {error}"
        )));
    }
    let step = plan
        .steps()
        .iter()
        .find(|step| matches!(step.kind, StepKind::GrowFilesystemOnline { .. }))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the online plan has no filesystem step".to_owned()),
                true,
            )
        })?;
    match super::filesystem::execute_online_step(platform, plan, step) {
        Ok(()) => Ok(None),
        // 表已提交；文件系统步的任何失败都只能按"已写、待恢复"报告。
        Err((error, _started)) => Err((error, true)),
    }
}

fn execute(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<Option<String>, (ExecError, bool)> {
    let has_fs = plan.steps().iter().any(|step| {
        matches!(
            step.kind,
            StepKind::MakeFilesystem { .. }
                | StepKind::GrowFilesystem { .. }
                | StepKind::ShrinkFilesystem { .. }
        )
    });
    let has_lvm = plan.steps().iter().any(|step| {
        matches!(
            step.kind,
            StepKind::LvmPvShrink { .. } | StepKind::LvmGrowChain { .. }
        )
    });
    let shrink = plan.steps().iter().any(|step| {
        matches!(
            step.kind,
            StepKind::ShrinkFilesystem { .. } | StepKind::LvmPvShrink { .. }
        )
    });
    if shrink {
        // 收缩：PV / 文件系统先缩，成功后才改表。
        if has_lvm {
            super::lvm::execute_plan(platform, target, plan)?;
        }
        if has_fs {
            super::filesystem::execute_plan(platform, target, plan)?;
        }
        super::table::execute_plan(platform, target, plan, true)
    } else {
        // 扩容：先提交分区表，再扩 PV / 文件系统。
        let unsynced = super::table::execute_plan(platform, target, plan, true)?;
        if has_fs {
            super::filesystem::execute_plan(platform, target, plan)?;
        }
        if has_lvm {
            super::lvm::execute_plan(platform, target, plan)?;
        }
        Ok(unsynced)
    }
}

/// 把执行错误归档为可报告的终态。
fn failure(error: ExecError, touched: bool) -> Outcome {
    if touched {
        return Outcome::needs_recovery(error.to_string());
    }
    let kind = match &error {
        ExecError::Unsupported(_) => FailureKind::Unsupported,
        ExecError::Record(_) | ExecError::IdentityChanged(_) | ExecError::InvalidPlan(_) => {
            FailureKind::Refused
        }
        ExecError::Platform(_) | ExecError::Io(_) => FailureKind::FailedBeforeWrite,
        ExecError::Postcondition(_) | ExecError::Injected(_) => FailureKind::Internal,
    };
    Outcome::unwritten(kind, error.to_string())
}
