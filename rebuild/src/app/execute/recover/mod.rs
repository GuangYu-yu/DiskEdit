//! `recover` 的唯一执行器：持锁、锁内重探身份、按记录回写原像或归档记录。
//!
//! B9 的准入顺序在这里落地：取记录目录锁 → 锁内重探身份 → 锁内重读记录 →
//! 再按纯恢复器的结论动作。回滚只读不写：逐条读目标当前字节并与原像核对
//! （B7 第五条 / B24），不符即拒绝并保留记录，绝不覆盖外部改动；清理与放弃
//! 都先回读再删除、删除后对目录 `fsync`（B23）。

use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::{Fingerprint, Strength};
use diskedit_core::record::{Frame, RecoveryPlan, recover};
use diskedit_core::region::Region;

use super::digest::Hasher;
use super::transaction::LOCK_FILE;
use super::{ExecError, FailureKind, Outcome};
use crate::app::decide::{ConfirmedPlan, StepKind};
use crate::app::observe::RecoverMode;
use crate::app::observe::records::{RECORD_FILE, hex16};
use crate::platform::{Platform, TargetHandle};

/// 归档文件名的前缀：`record.<N>.abandoned`，编号顺延且不覆盖。
const ABANDONED_PREFIX: &str = "record.";

/// 执行一份已确认的恢复计划。
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
    let mode = plan
        .steps()
        .iter()
        .find_map(|step| match &step.kind {
            StepKind::RecoverRecords { mode } => Some(*mode),
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
            | StepKind::GrowFilesystemOnline { .. }
            | StepKind::CopyPartition { .. }
            | StepKind::LvmPvShrink { .. }
            | StepKind::LvmGrowChain { .. }
            | StepKind::MovePartition { .. }
            | StepKind::RefreshKernelView => None,
        })
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the plan has no recovery step".to_owned()),
                false,
            )
        })?;
    let identity = plan.identity();
    let root = platform.state_root().ok_or_else(|| {
        (
            ExecError::Record("the host has no state directory".to_owned()),
            false,
        )
    })?;
    // B3：记录目录按目标身份派生；锁与记录同目录。
    let directory: PathBuf = root.join("images").join(hex16(identity));
    platform
        .create_directory_all(&directory)
        .map_err(|error| (ExecError::from(error), false))?;
    let _lock = platform
        .lock_state(&directory.join(LOCK_FILE))
        .map_err(|error| (ExecError::from(error), false))?;
    // B7 / B9：持锁后重探身份，计划指向的对象必须仍是当前对象。
    let observed = platform
        .identity(target)
        .ok()
        .filter(|fingerprint| fingerprint.strength() == Strength::Strong);
    match observed {
        Some(fingerprint) if fingerprint.matches(identity) => {}
        _ => {
            return Err((
                ExecError::IdentityChanged("target identity changed after planning".to_owned()),
                false,
            ));
        }
    }
    let file = directory.join(RECORD_FILE);
    let bytes = platform
        .read_file(&file)
        .map_err(|error| (ExecError::from(error), false))?;
    match mode {
        RecoverMode::Release { .. } => release(platform, &directory, &file, bytes, identity),
        mode @ (RecoverMode::Default | RecoverMode::ReplayTableOnly) => {
            rollback(platform, target, &directory, &file, bytes, identity, mode)
        }
    }
}

fn rollback(
    platform: &dyn Platform,
    target: &Path,
    directory: &Path,
    file: &Path,
    bytes: Option<Vec<u8>>,
    identity: Fingerprint,
    _mode: RecoverMode,
) -> Result<Outcome, (ExecError, bool)> {
    let Some(bytes) = bytes else {
        // 记录在执行层取锁前被清理：已无事可做。
        return Ok(Outcome::completed());
    };
    match recover(&bytes, identity) {
        RecoveryPlan::Absent => Ok(Outcome::completed()),
        RecoveryPlan::Committed => {
            clear(platform, directory, file, &bytes).map_err(|error| (error, false))?;
            Ok(Outcome::completed())
        }
        RecoveryPlan::Foreign { magic } => Err((
            ExecError::Record(format!(
                "record was not written by this implementation: {:?}",
                String::from_utf8_lossy(&magic)
            )),
            false,
        )),
        RecoveryPlan::NeedsManual { why } => Err((ExecError::Record(why), false)),
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            let plan = preimages(&frames).map_err(|error| (error, false))?;
            verify_against_preimage(platform, target, &plan).map_err(|error| (error, false))?;
            clear(platform, directory, file, &bytes).map_err(|error| (error, false))?;

            Ok(Outcome::rolled_back())
        }
    }
}

fn release(
    platform: &dyn Platform,
    directory: &Path,
    file: &Path,
    bytes: Option<Vec<u8>>,
    identity: Fingerprint,
) -> Result<Outcome, (ExecError, bool)> {
    let Some(bytes) = bytes else {
        return Ok(Outcome::completed());
    };
    // B8：放弃只看对象与归属。若记录能解码且属于别的载体，拒绝动它。
    if let Ok(decoded) = diskedit_core::record::decode(&bytes) {
        if !decoded.body.target.matches(identity) {
            return Err((
                ExecError::Record("the record belongs to a different target".to_owned()),
                false,
            ));
        }
    }
    abandon(platform, directory, file, &bytes).map_err(|error| (error, false))?;
    Ok(Outcome::completed())
}

/// 一条待回写的原像：区间与原始字节。
struct Preimage {
    region: Region,
    data: Vec<u8>,
}

/// 从记录帧里取出携带原像字节的区间，并核对登记摘要与字节一致。
fn preimages(frames: &[Frame]) -> Result<Vec<Preimage>, ExecError> {
    let mut out = Vec::new();
    for frame in frames {
        for region in &frame.regions {
            if region.data.is_empty() {
                continue;
            }
            if region.data.len() as u64 != region.len {
                return Err(ExecError::Record(
                    "the recorded preimage length and region length disagree".to_owned(),
                ));
            }
            let span = Region::sized(region.offset, region.len).map_err(|_| {
                ExecError::Record("the recorded preimage region is out of range".to_owned())
            })?;
            let mut hasher = Hasher::new();
            hasher.update(&region.data);
            if hasher.finish() != region.digest {
                return Err(ExecError::Record(
                    "the recorded preimage content and digest disagree".to_owned(),
                ));
            }
            out.push(Preimage {
                region: span,
                data: region.data.clone(),
            });
        }
    }
    Ok(out)
}

/// B7 第五条 / B24：逐条读目标当前字节并与原像核对。不符即拒绝、绝不覆盖。
///
/// 记录只持久化原像，不保存写入后的字节，因此重建的 `recover` 不会前向重放：
/// 盘面一旦已经离开原像，只能拒绝并把记录留给人工或 `--release`。
fn verify_against_preimage(
    platform: &dyn Platform,
    target: &Path,
    preimages: &[Preimage],
) -> Result<(), ExecError> {
    if preimages.is_empty() {
        return Ok(());
    }
    let handle = platform.open_target_read_only(target)?;
    for item in preimages {
        let current = read_exact(handle.as_ref(), item.region.start(), item.data.len())?;
        if current != item.data {
            return Err(ExecError::Record(format!(
                "target bytes at [{}..{}) differ from the recorded preimage — refusing to overwrite (B24)",
                item.region.start(),
                item.region.end()
            )));
        }
    }
    Ok(())
}

/// 从目标读满 `len` 字节；短读按环境故障处理。
fn read_exact(handle: &dyn TargetHandle, offset: u64, len: usize) -> Result<Vec<u8>, ExecError> {
    let mut buffer = vec![0u8; len];
    let mut filled = 0usize;
    while filled < len {
        let read = handle.read_at(offset + filled as u64, &mut buffer[filled..])?;
        if read == 0 {
            return Err(ExecError::Io(format!(
                "target ended before {len} bytes could be read at {offset}"
            )));
        }
        filled += read;
    }
    Ok(buffer)
}

/// B23 清理：回读必须仍是同一份记录，才删除并对目录 `fsync`。
fn clear(
    platform: &dyn Platform,
    directory: &Path,
    file: &Path,
    expected: &[u8],
) -> Result<(), ExecError> {
    let current = platform.read_file(file)?;
    if current.as_deref() != Some(expected) {
        return Err(ExecError::Record(
            "the record changed before it could be cleaned up".to_owned(),
        ));
    }
    platform.remove_state_file(file)?;
    platform.sync_directory(directory)?;
    Ok(())
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

/// 把执行错误归档为可报告的终态。
fn failure(error: ExecError, wrote: bool) -> Outcome {
    if wrote {
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

#[cfg(test)]
mod tests;
