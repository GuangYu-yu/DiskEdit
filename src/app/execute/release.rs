//! `release` 的唯一执行器：持锁、锁内重探身份、归档记录。
//!
//! 放弃不写目标字节（B8）：记录同目录归档为 `record.<N>.abandoned`，编号顺延
//! 不覆盖，目录 `fsync`。锁内现场与错误归档与 `undo` 共用。

use std::path::Path;

use super::undo::{failure, prepare};
use super::{ExecError, Outcome};
use crate::app::decide::ConfirmedPlan;
use crate::app::observe::RecoverMode;
use crate::platform::Platform;

/// 归档文件名的前缀：`record.<N>.abandoned`，编号顺延且不覆盖。
const ABANDONED_PREFIX: &str = "record.";

/// 执行一份已确认的放弃计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    match execute(platform, target, plan) {
        Ok(outcome) => outcome,
        Err((error, wrote)) => failure(error, wrote),
    }
}

fn execute(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<Outcome, (ExecError, bool)> {
    let mode = super::undo::mode_of(plan)?;
    match mode {
        RecoverMode::Release { .. } => {}
        RecoverMode::Undo | RecoverMode::ReplayTableOnly => {
            return Err((
                ExecError::InvalidPlan("the plan asks for a rollback, not a release".to_owned()),
                false,
            ));
        }
    }
    let identity = plan.identity();
    let site = prepare(platform, target, plan)?;
    release(platform, &site.directory, &site.file, site.bytes, identity)
}

/// B8 放弃：只看对象与归属，目标字节不变。
fn release(
    platform: &dyn Platform,
    directory: &Path,
    file: &Path,
    bytes: Option<Vec<u8>>,
    identity: diskedit_core::fingerprint::Fingerprint,
) -> Result<Outcome, (ExecError, bool)> {
    let Some(bytes) = bytes else {
        return Ok(Outcome::completed());
    };
    // B8：放弃只看对象与归属。若记录能解码且属于别的载体，拒绝动它。
    if let Ok(decoded) = diskedit_core::record::decode(&bytes)
        && !decoded.body.target.matches(identity)
    {
        return Err((
            ExecError::Record("the record belongs to a different target".to_owned()),
            false,
        ));
    }
    abandon(platform, directory, file, &bytes).map_err(|error| (error, false))?;
    Ok(Outcome::completed())
}

/// B8 放弃：同目录归档为 `record.<N>.abandoned`，编号顺延不覆盖，目录 `fsync`。
fn abandon(
    platform: &dyn Platform,
    directory: &Path,
    file: &Path,
    bytes: &[u8],
) -> Result<(), ExecError> {
    let mut number = 1u64;
    let archived = loop {
        let candidate = directory.join(format!("{ABANDONED_PREFIX}{number}.abandoned"));
        if platform.read_file(&candidate)?.is_none() {
            break candidate;
        }
        number += 1;
        if number == u64::MAX {
            return Err(ExecError::Record(
                "cannot find a free abandoned name".to_owned(),
            ));
        }
    };
    platform.write_state_file(&archived, bytes)?;
    platform.sync_directory(directory)?;
    platform.remove_state_file(file)?;
    platform.sync_directory(directory)?;
    Ok(())
}
