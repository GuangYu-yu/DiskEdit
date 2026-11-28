//! `release` 的唯一执行器：持锁、锁内重探身份、归档记录。
//!
//! 放弃不写目标字节：记录同目录归档为 `record.<N>.abandoned`，编号顺延
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
    let discard_replay = match mode {
        RecoverMode::Release { discard_replay } => discard_replay,
        RecoverMode::Undo | RecoverMode::ReplayTableOnly => {
            return Err((
                ExecError::InvalidPlan("the plan asks for a rollback, not a release".to_owned()),
                false,
            ));
        }
    };
    let identity = plan.identity();
    let site = prepare(platform, target, plan)?;
    let Some(bytes) = site.bytes else {
        return Ok(Outcome::completed());
    };
    let total_bytes = platform
        .open_target_read_only(target)
        .map_err(|error| (ExecError::from(error), false))?
        .len();
    release(
        platform,
        &site.directory,
        &site.file,
        bytes,
        identity,
        discard_replay,
        total_bytes,
    )
}

/// 锁内放弃记录：记录先过四道准入，再归档；目标字节不变。
fn release(
    platform: &dyn Platform,
    directory: &Path,
    file: &Path,
    bytes: Vec<u8>,
    identity: diskedit_core::fingerprint::Fingerprint,
    discard_replay: bool,
    total_bytes: u64,
) -> Result<Outcome, (ExecError, bool)> {
    // 锁内重读的记录可能已不是判定时看到的那一份：四道准入在这里重跑一遍。
    let plan = diskedit_core::record::recover(
        &bytes,
        diskedit_core::record::RecoveryExpectation::new(identity, total_bytes),
    );
    if let Err(refusal) =
        crate::app::decide::release::admit(plan, discard_replay, &file.display().to_string())
    {
        return Err((ExecError::Record(refusal.describe()), false));
    }
    abandon(platform, directory, file, &bytes).map_err(|error| (error, false))?;
    Ok(Outcome::completed())
}

/// 放弃：同目录归档为 `record.<N>.abandoned`，编号顺延不覆盖，目录 `fsync`。
fn abandon(
    platform: &dyn Platform,
    directory: &Path,
    file: &Path,
    bytes: &[u8],
) -> Result<(), ExecError> {
    let mut number = 1u64;
    let archived = loop {
        let candidate = directory.join(format!("{ABANDONED_PREFIX}{number}.abandoned"));
        if crate::app::observe::records::read_bytes(platform, &candidate)?.is_none() {
            break candidate;
        }
        number += 1;
        if number == u64::MAX {
            return Err(ExecError::Record(
                "cannot find a free abandoned name".to_owned(),
            ));
        }
    };
    platform.write_record(&archived, bytes)?;
    platform.remove_record(file)?;
    Ok(())
}
