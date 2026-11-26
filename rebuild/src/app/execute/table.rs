//! 整表写入的唯一执行器：核对表区未变、登记原像、经写通道落表、回读并提交记录。
//!
//! `new` 与 `add` / `delete` / `set` 写表时走同一条路径：判定层给出写段，这里只
//! 负责把段安全地落到独占句柄上。身份在持锁后重新核对，表区摘要与计划里的一致
//! 才允许写。任何中途失败都按"是否已写目标字节"归档为未写或待恢复。

use std::path::Path;

use diskedit_core::table::stamp::{stamp_regions, table_stamp};

use super::digest::read_region;
use super::transaction::RecordSession;
use super::{ExecError, FailureKind, Outcome, WriteChannel};
use crate::app::decide::step::TableSegment;
use crate::app::decide::{ConfirmedPlan, Step, StepKind};
use crate::platform::{Platform, TargetHandle};

/// 读原像与摘要时分段大小。
const CHUNK: usize = 1 << 20;

/// 执行一份已确认的整表写入计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    match execute_plan(platform, target, plan, true) {
        Ok(Some(why)) => Outcome::committed_unsynced(why),
        Ok(None) => Outcome::completed(),
        Err((error, wrote)) => failure(error, wrote),
    }
}

/// 表写事务主体；`resize` 也以它为表阶段，成功时返回内核视图未同步的原因（若有）。
///
/// `refresh` 为假时不在表提交后重读整盘：已挂载分区的在线路径随后用
/// `BLKPG_RESIZE_PARTITION` 按分区增量同步，整盘重读只会得到 `EBUSY`。
pub(super) fn execute_plan(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
    refresh: bool,
) -> Result<Option<String>, (ExecError, bool)> {
    let step = plan
        .steps()
        .iter()
        .find(|step| matches!(step.kind, StepKind::WriteTable { .. }))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the plan has no table write step".to_owned()),
                false,
            )
        })?;
    let (segment_size, stamp, segments) = table_parts(step)?;
    match super::filesystem::ensure_block_capability(platform, target) {
        Ok(()) => {}
        Err(error @ ExecError::Unsupported(_)) if refresh => {
            return execute_via_carrier(
                platform,
                target,
                plan,
                segment_size,
                stamp,
                segments,
                error,
            );
        }
        Err(error) => return Err((error, false)),
    }
    let mut handle = platform
        .open_target(target)
        .map_err(|error| (ExecError::from(error), false))?;
    if handle.len() < segment_size {
        return Err((
            ExecError::Io("the target is smaller than one sector".to_owned()),
            false,
        ));
    }
    let current = current_stamp(handle.as_mut(), segment_size).map_err(|error| (error, false))?;
    if current != stamp {
        return Err((
            ExecError::IdentityChanged(
                "the target bytes changed after the plan was made".to_owned(),
            ),
            false,
        ));
    }

    let mut session =
        RecordSession::prepare(platform, plan.identity()).map_err(|error| (error, false))?;
    // 持锁后重探身份：计划指向的对象必须仍是当前对象。
    if !super::identity_still_matches(platform, target, plan.identity()) {
        return Err((
            ExecError::IdentityChanged("target identity changed after planning".to_owned()),
            false,
        ));
    }

    let mut registered = Vec::with_capacity(segments.len());
    for segment in segments {
        let bytes =
            read_region(handle.as_mut(), segment.region, CHUNK).map_err(|error| (error, false))?;
        registered.push((segment.region, bytes, segment.bytes.clone()));
    }
    session
        .register(plan.request_digest(), &registered)
        .map_err(|error| (error, false))?;

    let token = session.token();
    let mut channel = WriteChannel::new(handle.as_mut(), &step.regions);
    let mut wrote = false;
    for segment in segments {
        channel
            .write(&token, segment.region, &segment.bytes)
            .map_err(|error| (error, wrote))?;
        wrote = true;
    }
    channel.barrier().map_err(|error| (error, wrote))?;
    super::postcondition::verify(&channel).map_err(|error| (error, wrote))?;
    drop(channel);

    session.commit().map_err(|error| (error, wrote))?;
    // 记录已提交；内核视图同步失败属于"持久状态已达成但视图未同步"。
    if !refresh {
        return Ok(None);
    }
    match handle.refresh_partitions() {
        Ok(()) => Ok(None),
        Err(error) => Ok(Some(format!(
            "the table is committed but the kernel view did not refresh: {error}"
        ))),
    }
}

/// 宿主缺块层能力时经虚拟机载体落表：字节由来宾写，锁/摘要/原像/记录留在宿主。
fn execute_via_carrier(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
    segment_size: u64,
    stamp: [u8; 32],
    segments: &[TableSegment],
    unsupported: ExecError,
) -> Result<Option<String>, (ExecError, bool)> {
    let carrier = match platform.carrier() {
        Ok(Some(carrier)) => carrier,
        Ok(None) => return Err((unsupported, false)),
        Err(error) => return Err((ExecError::from(error), false)),
    };
    let mut handle = platform
        .open_target(target)
        .map_err(|error| (ExecError::from(error), false))?;
    if handle.len() < segment_size {
        return Err((
            ExecError::Io("the target is smaller than one sector".to_owned()),
            false,
        ));
    }
    let current = current_stamp(handle.as_mut(), segment_size).map_err(|error| (error, false))?;
    if current != stamp {
        return Err((
            ExecError::IdentityChanged(
                "the target bytes changed after the plan was made".to_owned(),
            ),
            false,
        ));
    }
    let mut session =
        RecordSession::prepare(platform, plan.identity()).map_err(|error| (error, false))?;
    if !super::identity_still_matches(platform, target, plan.identity()) {
        return Err((
            ExecError::IdentityChanged("target identity changed after planning".to_owned()),
            false,
        ));
    }
    let mut registered = Vec::with_capacity(segments.len());
    for segment in segments {
        let bytes =
            read_region(handle.as_mut(), segment.region, CHUNK).map_err(|error| (error, false))?;
        registered.push((segment.region, bytes, segment.bytes.clone()));
    }
    session
        .register(plan.request_digest(), &registered)
        .map_err(|error| (error, false))?;

    let writes: Vec<crate::platform::CarrierWrite> = segments
        .iter()
        .map(|segment| crate::platform::CarrierWrite {
            offset: segment.region.start(),
            bytes: segment.bytes.clone(),
        })
        .collect();
    let request = crate::platform::DiskWriteRequest {
        disk: target.to_path_buf(),
        sector_size: segment_size,
        writes,
    };
    if let Err(error) = carrier.write_disk(&request) {
        return Err((ExecError::from(error), true));
    }
    session.commit().map_err(|error| (error, true))?;
    match handle.refresh_partitions() {
        Ok(()) => Ok(Some(
            "the table is committed through the VM carrier; the host kernel view was refreshed"
                .to_owned(),
        )),
        Err(error) => Ok(Some(format!(
            "the table is committed through the VM carrier but the host kernel view did not refresh: {error}"
        ))),
    }
}

/// 表写步骤的三元组：扇区大小、写前摘要、分段。
type TableWrite<'a> = (u64, [u8; 32], &'a [TableSegment]);

fn table_parts(step: &Step) -> Result<TableWrite<'_>, (ExecError, bool)> {
    match &step.kind {
        StepKind::WriteTable {
            segment_size,
            stamp,
            segments,
            ..
        } => Ok((*segment_size, *stamp, segments)),
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::AddPartition { .. }
        | StepKind::DeletePartition { .. }
        | StepKind::ResizePartition { .. }
        | StepKind::SetProperty { .. }
        | StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. }
        | StepKind::GrowFilesystemOnline { .. }
        | StepKind::ShrinkFilesystemOnline { .. }
        | StepKind::RebuildSwap { .. }
        | StepKind::CopyPartition { .. }
        | StepKind::LvmPvShrink { .. }
        | StepKind::LvmGrowChain { .. }
        | StepKind::MovePartition { .. }
        | StepKind::RefreshKernelView { .. }
        | StepKind::RecoverRecords { .. } => Err((
            ExecError::InvalidPlan("the step is not a table write".to_owned()),
            false,
        )),
    }
}

fn current_stamp(handle: &mut dyn TargetHandle, sector_size: u64) -> Result<[u8; 32], ExecError> {
    let mut sectors = Vec::new();
    for region in stamp_regions(handle.len(), sector_size) {
        sectors.push(read_region(handle, region, CHUNK)?);
    }
    let parts: Vec<&[u8]> = sectors.iter().map(Vec::as_slice).collect();
    Ok(table_stamp(&parts))
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
