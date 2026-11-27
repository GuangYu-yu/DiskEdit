//! 搬移与复制：先按块搬数据，再整表提交，全程用一条可续跑的记录。
//!
//! 数据步不登记原像（B6：搬移已写的数据不回滚），但要先落一条意图帧才能写；
//! 搬移每写完一块就追加一条进度帧（块游标与已写前缀摘要），续跑据此判断能否
//! 接上。表步仍走"先登记整扇区原像、再经唯一写通道落盘"的同一套约束。

use std::path::Path;

use diskedit_core::region::Region;
use diskedit_core::table::relocate::{self, ChunkSpan};
use diskedit_core::table::stamp::{stamp_regions, table_stamp};

use super::digest::{Hasher, read_region};
use super::transaction::RelocateSession;
use super::{ExecError, FailureKind, Outcome, WriteChannel};
use crate::app::decide::step::TableSegment;
use crate::app::decide::{ConfirmedPlan, Step, StepKind};
use crate::platform::{Platform, TargetHandle};

/// 读原像与摘要时分段大小。
const CHUNK: usize = 1 << 20;
/// 意图帧的格式标记。
const SPEC_MAGIC: &[u8] = b"diskedit-relocate-v1";

/// 执行一份搬移/复制计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    match execute(platform, target, plan) {
        Ok(Some(why)) => Outcome::committed_unsynced(why),
        Ok(None) => Outcome::completed(),
        Err((error, wrote)) => failure(error, wrote),
    }
}

/// 数据步抽出的几何。
struct DataStep {
    source_lba: u64,
    dest_lba: u64,
    sectors: u64,
    chunk: u64,
    /// 目标起点减源起点。
    shift: i64,
    /// 搬移（可续跑）还是复制（不续跑）。
    resumable: bool,
    regions: Vec<Region>,
}

fn execute(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<Option<String>, (ExecError, bool)> {
    let (table_index, table) = plan
        .steps()
        .iter()
        .enumerate()
        .find(|(_, step)| matches!(step.kind, StepKind::WriteTable { .. }))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the plan has no table write step".to_owned()),
                false,
            )
        })?;
    let (segment_size, stamp, segments) = table_parts(table)?;
    let data = data_step(plan, segment_size)?;
    let data_index = plan
        .steps()
        .iter()
        .position(|step| {
            matches!(
                step.kind,
                StepKind::MovePartition { .. } | StepKind::CopyPartition { .. }
            )
        })
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the plan has no copy step".to_owned()),
                false,
            )
        })?;
    let sector_size = segment_size;
    let source_bytes = data
        .source_lba
        .checked_add(data.sectors)
        .and_then(|end| end.checked_mul(sector_size))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the source range overflows".to_owned()),
                false,
            )
        })?;
    let dest_bytes = data
        .dest_lba
        .checked_add(data.sectors)
        .and_then(|end| end.checked_mul(sector_size))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the target range overflows".to_owned()),
                false,
            )
        })?;

    let mut handle = platform
        .open_target(target)
        .map_err(|error| (ExecError::from(error), false))?;
    let needed = source_bytes.max(dest_bytes).max(segment_size);
    if handle.len() < needed {
        return Err((
            ExecError::Io(format!(
                "the target holds {} bytes but the job needs {needed}",
                handle.len()
            )),
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

    let spec = spec_bytes(&data, sector_size, handle.len());
    let (mut session, recorded) =
        RelocateSession::prepare(platform, plan.identity(), plan.digest(), &spec)?;
    if !super::identity_still_matches(platform, target, plan.identity()) {
        return Err((
            ExecError::IdentityChanged("target identity changed after planning".to_owned()),
            false,
        ));
    }

    let layout =
        relocate::layout(data.sectors, sector_size, data.shift, data.chunk).ok_or_else(|| {
            (
                ExecError::InvalidPlan("the chunk layout is degenerate".to_owned()),
                false,
            )
        })?;
    let spans = layout.spans(data.source_lba);
    let total = u64::try_from(spans.len()).expect("块数可表示");

    let mut cursor = 0u64;
    if let Some((recorded_cursor, recorded_digest)) = recorded {
        if recorded_cursor > total {
            return Err((
                ExecError::Record("the record's chunk cursor is past the job's end".to_owned()),
                false,
            ));
        }
        let mut prefix = Hasher::new();
        hash_prefix(
            handle.as_mut(),
            &spans,
            recorded_cursor,
            sector_size,
            data.shift,
            &mut prefix,
        )?;
        match relocate::resume(Some((recorded_cursor, recorded_digest)), prefix.finish()) {
            relocate::Resume::Start => cursor = 0,
            relocate::Resume::Continue { cursor: at } => cursor = at,
            relocate::Resume::Mismatch => {
                let overlaps =
                    relocate::relocation_overlaps(data.shift.unsigned_abs(), data.sectors);
                if overlaps {
                    return Err((
                        ExecError::Record(
                            "the target prefix does not match the record's written prefix; a \
                             self-overlapping move cannot restart without rereading overwritten \
                             source bytes — inspect and release the record"
                                .to_owned(),
                        ),
                        false,
                    ));
                }
                cursor = 0;
            }
        }
    }
    if session.table_registered() && cursor < total {
        return Err((
            ExecError::Record(
                "the record already holds table preimages but the copy is not finished".to_owned(),
            ),
            false,
        ));
    }

    let mut wrote = false;
    if cursor < total {
        let data_number = u64::try_from(data_index).expect("步骤号可表示");
        let token = session
            .token(data_number, &data.regions)
            .map_err(|(error, _)| (error, wrote))?;
        let mut channel = WriteChannel::new(
            handle.as_mut(),
            session.id(),
            token,
            data_number,
            &data.regions,
        )
        .map_err(|error| (error, wrote))?;
        let mut rolling = Hasher::new();
        hash_prefix_in_channel(
            &channel,
            &spans,
            cursor,
            sector_size,
            data.shift,
            &mut rolling,
        )?;
        for (index, span) in spans.iter().enumerate().skip(usize_from(cursor)) {
            let source = lba_region(span.first_lba, span.len_lba, sector_size)?;
            let destination = shifted(span, data.shift, sector_size)?;
            let bytes = channel
                .read_exact(source.start(), usize_from(source.len()))
                .map_err(|error| (error, wrote))?;
            channel
                .write(destination, &bytes)
                .map_err(|error| (error, wrote))?;
            wrote = true;
            rolling.update(&bytes);
            if data.resumable {
                let at = u64::try_from(index + 1).expect("块下标可表示");
                session
                    .checkpoint(at, rolling.finish())
                    .map_err(|(error, _)| (error, wrote))?;
            }
        }
        channel.barrier().map_err(|error| (error, wrote))?;
        channel.verify_written().map_err(|error| (error, wrote))?;
    }

    let mut registered = Vec::with_capacity(segments.len());
    for segment in segments {
        let bytes =
            read_region(handle.as_mut(), segment.region, CHUNK).map_err(|error| (error, wrote))?;
        registered.push((segment.region, bytes, segment.bytes.clone()));
    }
    session
        .register_table(&registered)
        .map_err(|(error, _)| (error, wrote))?;

    let table_number = u64::try_from(table_index).expect("步骤号可表示");
    let token = session
        .token(table_number, &table.regions)
        .map_err(|(error, _)| (error, wrote))?;
    let mut channel = WriteChannel::new(
        handle.as_mut(),
        session.id(),
        token,
        table_number,
        &table.regions,
    )
    .map_err(|error| (error, wrote))?;
    for segment in segments {
        channel
            .write(segment.region, &segment.bytes)
            .map_err(|error| (error, wrote))?;
        wrote = true;
    }
    channel.barrier().map_err(|error| (error, wrote))?;
    super::postcondition::verify(&channel).map_err(|error| (error, wrote))?;
    drop(channel);

    session.commit().map_err(|(error, _)| (error, wrote))?;
    match handle.refresh_partitions() {
        Ok(()) => Ok(None),
        Err(error) => Ok(Some(format!(
            "the table is committed but the kernel view did not refresh: {error}"
        ))),
    }
}

fn data_step(plan: &ConfirmedPlan, sector_size: u64) -> Result<DataStep, (ExecError, bool)> {
    for step in plan.steps() {
        match &step.kind {
            StepKind::MovePartition {
                source_lba,
                offset_sectors,
                chunk,
                ..
            } => {
                let dest = i128::from(*source_lba) + i128::from(*offset_sectors);
                let dest_lba = u64::try_from(dest).map_err(|_| {
                    (
                        ExecError::InvalidPlan("the move target LBA is out of range".to_owned()),
                        false,
                    )
                })?;
                let regions = step.regions.clone();
                let sectors = regions.first().map(|region| region.len()).unwrap_or(0);
                if sectors == 0 || sector_size == 0 {
                    return Err((
                        ExecError::InvalidPlan("the move step declares no region".to_owned()),
                        false,
                    ));
                }
                return Ok(DataStep {
                    source_lba: *source_lba,
                    dest_lba,
                    sectors: sectors / sector_size,
                    chunk: *chunk,
                    shift: *offset_sectors,
                    resumable: true,
                    regions,
                });
            }
            StepKind::CopyPartition {
                source_lba,
                start_lba,
                sectors,
                chunk,
                ..
            } => {
                let shift = i128::from(*start_lba) - i128::from(*source_lba);
                let shift = i64::try_from(shift).map_err(|_| {
                    (
                        ExecError::InvalidPlan("the copy shift is out of range".to_owned()),
                        false,
                    )
                })?;
                return Ok(DataStep {
                    source_lba: *source_lba,
                    dest_lba: *start_lba,
                    sectors: *sectors,
                    chunk: *chunk,
                    shift,
                    resumable: false,
                    regions: step.regions.clone(),
                });
            }
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
            | StepKind::ShrinkFilesystemOnline { .. }
            | StepKind::RebuildSwap { .. }
            | StepKind::LvmPvShrink { .. }
            | StepKind::LvmGrowChain { .. }
            | StepKind::RefreshKernelView { .. }
            | StepKind::RecoverRecords { .. } => {}
        }
    }
    Err((
        ExecError::InvalidPlan("the plan has no relocation step".to_owned()),
        false,
    ))
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

fn lba_region(lba: u64, sectors: u64, sector_size: u64) -> Result<Region, (ExecError, bool)> {
    lba.checked_mul(sector_size)
        .and_then(|offset| sectors.checked_mul(sector_size).map(|len| (offset, len)))
        .and_then(|(offset, len)| Region::sized(offset, len).ok())
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("a relocation region overflows".to_owned()),
                false,
            )
        })
}

fn shifted(span: &ChunkSpan, shift: i64, sector_size: u64) -> Result<Region, (ExecError, bool)> {
    let dest = i128::from(span.first_lba) + i128::from(shift);
    let lba = u64::try_from(dest).map_err(|_| {
        (
            ExecError::InvalidPlan("a destination LBA is out of range".to_owned()),
            false,
        )
    })?;
    lba_region(lba, span.len_lba, sector_size)
}

/// 把目标里按拷贝顺序已写的 `upto` 个块重新摘要，用于续跑比对。
fn hash_prefix(
    handle: &mut dyn TargetHandle,
    spans: &[ChunkSpan],
    upto: u64,
    sector_size: u64,
    shift: i64,
    hasher: &mut Hasher,
) -> Result<(), (ExecError, bool)> {
    for span in spans.iter().take(usize_from(upto)) {
        let region = shifted(span, shift, sector_size)?;
        let bytes = read_region(handle, region, CHUNK).map_err(|error| (error, false))?;
        hasher.update(&bytes);
    }
    Ok(())
}

fn hash_prefix_in_channel(
    channel: &WriteChannel<'_>,
    spans: &[ChunkSpan],
    upto: u64,
    sector_size: u64,
    shift: i64,
    hasher: &mut Hasher,
) -> Result<(), (ExecError, bool)> {
    for span in spans.iter().take(usize_from(upto)) {
        let region = shifted(span, shift, sector_size)?;
        let bytes = channel
            .read_exact(region.start(), usize_from(region.len()))
            .map_err(|error| (error, false))?;
        hasher.update(&bytes);
    }
    Ok(())
}

fn usize_from(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// 序列化本次搬移的几何快照，写进意图帧；续跑要求逐字节相符。
fn spec_bytes(data: &DataStep, sector_size: u64, disk_len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(SPEC_MAGIC.len() + 1 + 6 * 8);
    out.extend_from_slice(SPEC_MAGIC);
    out.push(u8::from(data.resumable));
    for value in [
        data.source_lba,
        data.sectors,
        data.dest_lba,
        data.chunk,
        sector_size,
        disk_len,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
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
