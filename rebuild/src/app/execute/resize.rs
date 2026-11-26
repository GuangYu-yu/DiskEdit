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

fn execute(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<Option<String>, (ExecError, bool)> {
    let shrink = plan
        .steps()
        .iter()
        .any(|step| matches!(step.kind, StepKind::ShrinkFilesystem { .. }));
    if shrink {
        // 收缩：文件系统先缩，成功后才改表。
        super::filesystem::execute_plan(platform, target, plan)?;
        super::table::execute_plan(platform, target, plan)
    } else {
        // 扩容：先提交分区表，再扩文件系统。
        let unsynced = super::table::execute_plan(platform, target, plan)?;
        super::filesystem::execute_plan(platform, target, plan)?;
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
