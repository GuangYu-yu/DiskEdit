//! `undo` 的唯一执行器：持锁、锁内重探身份、按记录回写原像。
//!
//! B9 的准入顺序在这里落地：取记录目录锁 → 锁内重探身份 → 锁内重读记录 →
//! 再按纯恢复器的结论动作。回滚先做只读判定（B7 第五条 / B24）：当前字节必须
//! 是本工具写后的样子、停在写前/写后之间的唯一断点，或已经是原像；三者之外一律
//! 拒绝并保留记录，绝不覆盖外部改动。判定通过后逆序写回原像并读回核对（B6）。
//! 清理先回读再删除、删除后对目录 `fsync`（B23）。`replay` 复用这里的回放通路，
//! `release` 复用这里的锁内现场准备与错误归档。

use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{Frame, RecoveryPlan, recover};
use diskedit_core::region::Region;

use super::digest::Hasher;
use super::transaction::LOCK_FILE;
use super::{ExecError, FailureKind, Outcome};
use crate::app::decide::{ConfirmedPlan, StepKind};
use crate::app::observe::RecoverMode;
use crate::app::observe::records::{RECORD_FILE, hex16};
use crate::platform::{Platform, TargetHandle};

/// 执行一份已确认的回滚计划。
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
    let mode = mode_of(plan)?;
    match mode {
        RecoverMode::Undo | RecoverMode::ReplayTableOnly => {}
        RecoverMode::Release { .. } => {
            return Err((
                ExecError::InvalidPlan("the plan asks for a release, not a rollback".to_owned()),
                false,
            ));
        }
    }
    let site = prepare(platform, target, plan)?;
    let identity = plan.identity();
    rollback(platform, target, &site.file, site.bytes, identity)
}

/// 锁内现场：记录目录、记录文件与记录字节。回放与放弃共用这一段准入。
pub(super) struct Site {
    /// 记录目录（含锁文件）。
    pub(super) directory: PathBuf,
    /// 记录文件路径。
    pub(super) file: PathBuf,
    /// 锁内读到的记录字节；没有记录时为 `None`。
    pub(super) bytes: Option<Vec<u8>>,
}

/// 锁内现场准入：建目录、取锁、重探身份、读记录。
pub(super) fn prepare(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<Site, (ExecError, bool)> {
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
    if !super::identity_still_matches(platform, target, identity) {
        return Err((
            ExecError::IdentityChanged("target identity changed after planning".to_owned()),
            false,
        ));
    }
    let file = directory.join(RECORD_FILE);
    let bytes = crate::app::observe::records::read_bytes(platform, &file)
        .map_err(|error| (ExecError::from(error), false))?;
    Ok(Site {
        directory,
        file,
        bytes,
    })
}

pub(super) fn mode_of(plan: &ConfirmedPlan) -> Result<RecoverMode, (ExecError, bool)> {
    plan.steps()
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
            | StepKind::RebuildSwap { .. }
            | StepKind::GrowFilesystemOnline { .. }
            | StepKind::ShrinkFilesystemOnline { .. }
            | StepKind::CopyPartition { .. }
            | StepKind::LvmPvShrink { .. }
            | StepKind::LvmGrowChain { .. }
            | StepKind::MovePartition { .. }
            | StepKind::RefreshKernelView { .. } => None,
        })
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the plan has no recovery step".to_owned()),
                false,
            )
        })
}

fn rollback(
    platform: &dyn Platform,
    target: &Path,
    file: &Path,
    bytes: Option<Vec<u8>>,
    identity: Fingerprint,
) -> Result<Outcome, (ExecError, bool)> {
    let Some(bytes) = bytes else {
        // 记录在执行层取锁前被清理：已无事可做。
        return Ok(Outcome::completed());
    };
    match recover(&bytes, identity) {
        RecoveryPlan::Absent => Ok(Outcome::completed()),
        RecoveryPlan::Committed => {
            clear(platform, file, &bytes).map_err(|error| (error, false))?;
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
            let pending = admit_replay(platform, target, &plan).map_err(|error| (error, false))?;
            let wrote = if pending {
                replay(platform, target, &plan)?
            } else {
                false
            };
            clear(platform, file, &bytes).map_err(|error| (error, wrote))?;

            Ok(Outcome::rolled_back())
        }
    }
}

/// 一条待回写的原像：区间、原始字节与本步写后字节。
struct Preimage {
    region: Region,
    data: Vec<u8>,
    after: Vec<u8>,
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
            if !region.after.is_empty() && region.after.len() != region.data.len() {
                return Err(ExecError::Record(
                    "the recorded preimage and postimage lengths disagree".to_owned(),
                ));
            }
            out.push(Preimage {
                region: span,
                data: region.data.clone(),
                after: region.after.clone(),
            });
        }
    }
    Ok(out)
}

/// 当前字节相对原像/写后字节的归类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordedWrite {
    /// 已是本步写后的样子。
    After,
    /// 已是原文。
    Original,
    /// 本工具的写在区间内停下。
    Torn { cut: usize },
}

impl RecordedWrite {
    /// 前缀与后缀分别计算；一次写只留一个切点。
    fn of(now: &[u8], after: &[u8], pre: &[u8]) -> Option<Self> {
        if now == after {
            return Some(Self::After);
        }
        if now == pre {
            return Some(Self::Original);
        }
        if after.is_empty() || now.len() != after.len() || now.len() != pre.len() {
            return None;
        }
        let prefix =
            |of: &[u8], front: &[u8]| of.iter().zip(front).take_while(|(a, b)| a == b).count();
        let suffix = |of: &[u8], back: &[u8]| {
            of.iter()
                .zip(back.iter().rev())
                .take_while(|(a, b)| a == b)
                .count()
        };
        let fits = |p: usize, s: usize, len: usize| p + s >= len;
        let forward = (prefix(after, now), suffix(pre, now));
        let backward = (prefix(pre, now), suffix(after, now));
        match (
            fits(forward.0, forward.1, now.len()),
            fits(backward.0, backward.1, now.len()),
        ) {
            (true, true) if backward.0 > forward.0 => Some(Self::Torn { cut: backward.0 }),
            (true, _) => Some(Self::Torn { cut: forward.0 }),
            (false, true) => Some(Self::Torn { cut: backward.0 }),
            (false, false) => None,
        }
    }
}

/// 一次写只在一个位置停下；两处断点意味着盘面被外部改过。
#[derive(Default)]
struct TearLog {
    first: Option<u64>,
    conflict: bool,
}

impl TearLog {
    fn record(&mut self, at: u64) -> Result<(), u64> {
        let Some(first) = self.first else {
            self.first = Some(at);
            return Ok(());
        };
        if self.conflict || first != at {
            self.conflict = true;
            return Err(first);
        }
        Ok(())
    }
}

/// 把逆序回放中原先写过的区间叠加到读出字节上。
fn read_virtual(
    mut read: impl FnMut(u64, usize) -> Result<Vec<u8>, ExecError>,
    offset: u64,
    len: usize,
    patches: &[(u64, Vec<u8>)],
) -> Result<Vec<u8>, ExecError> {
    let mut buffer = read(offset, len)?;
    for (patch_at, patch) in patches {
        let patch_end = patch_at.saturating_add(patch.len() as u64);
        let overlap_start = (*patch_at).max(offset);
        let overlap_end = patch_end.min(offset + len as u64);
        if overlap_start >= overlap_end {
            continue;
        }
        let copy = (overlap_end - overlap_start) as usize;
        let from = (overlap_start - patch_at) as usize;
        let to = (overlap_start - offset) as usize;
        buffer[to..to + copy].copy_from_slice(&patch[from..from + copy]);
    }
    Ok(buffer)
}

/// 回滚完成后的整段字节：先写的条目后叠加，因此逆序覆盖。
fn rolled_back_image(preimages: &[Preimage]) -> Result<Vec<(u64, Vec<u8>)>, ExecError> {
    let mut image: Vec<(u64, Vec<u8>)> = Vec::new();
    for item in preimages.iter().rev() {
        let offset = item.region.start();
        let mut merged = item.data.clone();
        let end = offset + merged.len() as u64;
        // 让新段覆盖已有段的重叠部分。
        let mut next: Vec<(u64, Vec<u8>)> = Vec::with_capacity(image.len() + 1);
        for (at, bytes) in &image {
            let other_end = at + bytes.len() as u64;
            if other_end <= offset || *at >= end {
                next.push((*at, bytes.clone()));
                continue;
            }
            if *at < offset {
                next.push((*at, bytes[..(offset - at) as usize].to_vec()));
            }
            if other_end > end {
                next.push((end, bytes[(end - at) as usize..].to_vec()));
            }
        }
        next.push((offset, std::mem::take(&mut merged)));
        next.sort_by_key(|(at, _)| *at);
        image = next;
    }
    Ok(image)
}

fn target_matches_image(
    mut read: impl FnMut(u64, usize) -> Result<Vec<u8>, ExecError>,
    image: &[(u64, Vec<u8>)],
) -> Result<bool, ExecError> {
    for (offset, expected) in image {
        if read(*offset, expected.len())? != *expected {
            return Ok(false);
        }
    }
    Ok(true)
}

/// B7 / B24：只读判定"盘面是否还需要回写"。返回 `true` 表示至少一条要写。
///
/// 每条的当前字节必须是原像、写后字节，或停在两者之间唯一断点的撕裂值；
/// 其余情况说明盘面被外部改过，拒绝并保留记录。
fn admit_replay(
    platform: &dyn Platform,
    target: &Path,
    preimages: &[Preimage],
) -> Result<bool, ExecError> {
    if preimages.is_empty() {
        return Ok(false);
    }
    let handle = platform.open_target_read_only(target)?;
    let image = rolled_back_image(preimages)?;
    if target_matches_image(|at, len| read_exact(handle.as_ref(), at, len), &image)? {
        return Ok(false);
    }
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut tears = TearLog::default();
    let mut pending = false;
    for item in preimages.iter().rev() {
        let offset = item.region.start();
        let now = read_virtual(
            |at, len| read_exact(handle.as_ref(), at, len),
            offset,
            item.data.len(),
            &patches,
        )?;
        if now != item.data {
            let torn = match RecordedWrite::of(&now, &item.after, &item.data) {
                Some(RecordedWrite::After) => None,
                Some(RecordedWrite::Torn { cut }) => Some(cut),
                Some(RecordedWrite::Original) => {
                    patches.push((offset, item.data.clone()));
                    continue;
                }
                None => {
                    return Err(ExecError::Record(format!(
                        "target bytes at [{}..{}) no longer hold the recorded preimage or the bytes this tool wrote there — the disk changed outside this tool; replaying would overwrite that change (B24)",
                        item.region.start(),
                        item.region.end()
                    )));
                }
            };
            if let Some(cut) = torn
                && let Err(first) = tears.record(offset + cut as u64)
            {
                return Err(ExecError::Record(format!(
                    "an interrupted write shows up at offset {first} and again at offset {} — one write stops in one place, so two separate break points mean the disk changed outside this tool; refusing to replay (B24)",
                    offset + cut as u64
                )));
            }
            pending = true;
        }
        patches.push((offset, item.data.clone()));
    }
    Ok(pending)
}

/// 逆序写回原像，随后按并集图像读回核对。首个字节写出去之后的任何失败都按待恢复报告。
fn replay(
    platform: &dyn Platform,
    target: &Path,
    preimages: &[Preimage],
) -> Result<bool, (ExecError, bool)> {
    let mut handle = platform
        .open_target(target)
        .map_err(|error| (ExecError::from(error), false))?;
    let declared: Vec<Region> = preimages.iter().map(|item| item.region).collect();
    // 回滚写入也走唯一写通道：原始写原语只留在通道内部，授权来自记录里的原像。
    let mut channel = super::WriteChannel::for_recovery(handle.as_mut(), &declared);
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut wrote = false;
    for item in preimages.iter().rev() {
        let offset = item.region.start();
        let now = read_virtual(
            |at, len| channel.read_exact(at, len),
            offset,
            item.data.len(),
            &patches,
        )
        .map_err(|error| (error, wrote))?;
        if now == item.data {
            patches.push((offset, item.data.clone()));
            continue;
        }
        // 判定与写之间不再留窗口：这里重新按同一判据核对。
        if RecordedWrite::of(&now, &item.after, &item.data).is_none() {
            return Err((
                ExecError::Record(format!(
                    "target bytes at [{}..{}) changed while replaying — refusing to overwrite the foreign edit (B24)",
                    item.region.start(),
                    item.region.end()
                )),
                wrote,
            ));
        }
        channel
            .write(item.region, &item.data)
            .map_err(|error| (error, wrote))?;
        wrote = true;
        patches.push((offset, item.data.clone()));
    }
    channel.barrier().map_err(|error| (error, wrote))?;
    let image = rolled_back_image(preimages).map_err(|error| (error, wrote))?;
    if !target_matches_image(|at, len| channel.read_exact(at, len), &image)
        .map_err(|error| (error, wrote))?
    {
        return Err((
            ExecError::Io(
                "the rollback wrote the preimages but the target holds different bytes".to_owned(),
            ),
            wrote,
        ));
    }
    Ok(wrote)
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
fn clear(platform: &dyn Platform, file: &Path, expected: &[u8]) -> Result<(), ExecError> {
    let current = crate::app::observe::records::read_bytes(platform, file)?;
    if current.as_deref() != Some(expected) {
        return Err(ExecError::Record(
            "the record changed before it could be cleaned up".to_owned(),
        ));
    }
    platform.remove_record(file)?;
    Ok(())
}

/// 把执行错误归档为可报告的终态。
pub(super) fn failure(error: ExecError, wrote: bool) -> Outcome {
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
