//! 搬移与复制：先按块搬数据，再整表提交，全程用一条可续跑的记录。
//!
//! 数据步不登记原像（B6：搬移已写的数据不回滚），但要先落一条意图帧才能写；
//! 搬移每写完一块就追加一条进度帧（块游标与已写前缀摘要），续跑据此判断能否
//! 接上。表步仍走"先登记整扇区原像、再经唯一写通道落盘"的同一套约束。

use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::{Fingerprint, Strength};
use diskedit_core::record::{self, Frame, FrameKind, Preimage, RecordBody};
use diskedit_core::region::Region;
use diskedit_core::table::relocate::{self, ChunkSpan};
use diskedit_core::table::stamp::{stamp_regions, table_stamp};

use super::digest::{Hasher, read_region};
use super::transaction::{LOCK_FILE, Mint};
use super::{ExecError, FailureKind, Outcome, Recorded, WriteChannel};
use crate::app::decide::step::TableSegment;
use crate::app::decide::{ConfirmedPlan, Step, StepKind};
use crate::app::observe::records::{RECORD_FILE, hex16};
use crate::platform::{Platform, StateLock, TargetHandle};

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
    let table = plan
        .steps()
        .iter()
        .find(|step| matches!(step.kind, StepKind::WriteTable { .. }))
        .ok_or_else(|| {
            (
                ExecError::InvalidPlan("the plan has no table write step".to_owned()),
                false,
            )
        })?;
    let (segment_size, stamp, segments) = table_parts(table)?;
    let data = data_step(plan, segment_size)?;
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
        Session::prepare(platform, plan.identity(), plan.digest(), &spec)?;
    let observed = platform
        .identity(target)
        .ok()
        .filter(|fingerprint| fingerprint.strength() == Strength::Strong);
    match observed {
        Some(fingerprint) if fingerprint.matches(plan.identity()) => {}
        _ => {
            return Err((
                ExecError::IdentityChanged("target identity changed after planning".to_owned()),
                false,
            ));
        }
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
    let token = session.token();

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
        let mut channel = WriteChannel::new(handle.as_mut(), &data.regions);
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
                .write(&token, destination, &bytes)
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

    let mut channel = WriteChannel::new(handle.as_mut(), &table.regions);
    for segment in segments {
        channel
            .write(&token, segment.region, &segment.bytes)
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

/// 一次搬移/复制的记录现场。
struct Session<'a> {
    platform: &'a dyn Platform,
    directory: PathBuf,
    file: PathBuf,
    _lock: Box<dyn StateLock>,
    body: RecordBody,
    frames: Vec<Frame>,
    mint: Mint,
    table_registered: bool,
}

/// 续跑点：已完成进度与最近一帧摘要。
type ResumePoint = (u64, [u8; 16]);

/// 执行失败与"目标是否可能已被写过"的成对结果。
type ExecFailure = (ExecError, bool);

impl<'a> Session<'a> {
    /// 取锁、读出可续跑进度；没有记录时先落一条意图帧。
    fn prepare(
        platform: &'a dyn Platform,
        identity: Fingerprint,
        plan_digest: [u8; 32],
        spec: &[u8],
    ) -> Result<(Self, Option<ResumePoint>), ExecFailure> {
        let root = platform.state_root().ok_or_else(|| {
            (
                ExecError::Record("the host has no state directory".to_owned()),
                false,
            )
        })?;
        let directory = root.join("images").join(hex16(identity));
        platform
            .create_directory_all(&directory)
            .map_err(|error| (ExecError::from(error), false))?;
        let lock = platform
            .lock_state(&directory.join(LOCK_FILE))
            .map_err(|error| (ExecError::from(error), false))?;
        let file = directory.join(RECORD_FILE);
        let body = RecordBody {
            target: identity,
            geometry_generation: 0,
            plan_digest,
        };

        let existing = platform
            .read_file(&file)
            .map_err(|error| (ExecError::from(error), false))?;
        if let Some(bytes) = existing.filter(|bytes| !bytes.is_empty()) {
            let decoded = record::decode(&bytes).map_err(|error| {
                (
                    ExecError::Record(format!("the existing record is unreadable: {error}")),
                    false,
                )
            })?;
            if !decoded.body.target.matches(identity) {
                return Err((
                    ExecError::Record("the record targets a different device".to_owned()),
                    false,
                ));
            }
            if decoded.body.plan_digest != plan_digest {
                return Err((
                    ExecError::Record("the existing record belongs to a different plan".to_owned()),
                    false,
                ));
            }
            let Some(Frame {
                kind: FrameKind::Intent,
                intent,
                ..
            }) = decoded.frames.first()
            else {
                return Err((
                    ExecError::Record("the record does not begin with an intent".to_owned()),
                    false,
                ));
            };
            if intent.as_slice() != spec {
                return Err((
                    ExecError::Record(
                        "the record's relocation geometry does not match this request".to_owned(),
                    ),
                    false,
                ));
            }
            let resume = decoded.frames.iter().rev().find_map(|frame| {
                if frame.kind == FrameKind::Progress && frame.intent.len() == 24 {
                    Some(progress_of(&frame.intent))
                } else {
                    None
                }
            });
            let table_registered = decoded
                .frames
                .iter()
                .any(|frame| frame.kind == FrameKind::Preimage);
            let frames = decoded.frames;
            return Ok((
                Self {
                    platform,
                    directory,
                    file,
                    _lock: lock,
                    body,
                    frames,
                    mint: Mint::new(),
                    table_registered,
                },
                resume,
            ));
        }

        let intent = Frame {
            seq: 0,
            kind: FrameKind::Intent,
            completed: false,
            irreversible: false,
            intent: spec.to_vec(),
            regions: Vec::new(),
        };
        let frames = vec![intent];
        let encoded = record::encode(&body, &frames).map_err(|error| {
            (
                ExecError::Record(format!("the record cannot be encoded: {error}")),
                false,
            )
        })?;
        platform
            .write_state_file(&file, &encoded)
            .map_err(|error| (ExecError::from(error), false))?;
        platform
            .sync_directory(&directory)
            .map_err(|error| (ExecError::from(error), false))?;
        Ok((
            Self {
                platform,
                directory,
                file,
                _lock: lock,
                body,
                frames,
                mint: Mint::new(),
                table_registered: false,
            },
            None,
        ))
    }

    fn token(&self) -> Recorded {
        Recorded::mint(&self.mint, 1)
    }

    fn table_registered(&self) -> bool {
        self.table_registered
    }

    /// 追加一条进度帧：块游标与已写前缀摘要。
    fn checkpoint(&mut self, cursor: u64, digest: [u8; 16]) -> Result<(), (ExecError, bool)> {
        let mut intent = Vec::with_capacity(24);
        intent.extend_from_slice(&cursor.to_le_bytes());
        intent.extend_from_slice(&digest);
        self.append(Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Progress,
            completed: true,
            irreversible: true,
            intent,
            regions: Vec::new(),
        })
    }

    /// 登记表区原像与写后字节；已有原像帧时复用。
    fn register_table(
        &mut self,
        segments: &[(Region, Vec<u8>, Vec<u8>)],
    ) -> Result<(), (ExecError, bool)> {
        if self.table_registered {
            return Ok(());
        }
        let frame = Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Preimage,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: segments
                .iter()
                .map(|(region, before, after)| preimage(*region, before, after))
                .collect(),
        };
        self.append(frame)?;
        self.table_registered = true;
        Ok(())
    }

    /// 追加提交帧、落盘并删除记录。
    fn commit(&mut self) -> Result<(), (ExecError, bool)> {
        self.append(Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Committed,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: Vec::new(),
        })?;
        self.platform
            .remove_state_file(&self.file)
            .map_err(|error| (ExecError::from(error), false))?;
        self.platform
            .sync_directory(&self.directory)
            .map_err(|error| (ExecError::from(error), false))?;
        Ok(())
    }

    fn append(&mut self, frame: Frame) -> Result<(), (ExecError, bool)> {
        let mut frames = self.frames.clone();
        frames.push(frame);
        let encoded = record::encode(&self.body, &frames).map_err(|error| {
            (
                ExecError::Record(format!("the record cannot be encoded: {error}")),
                false,
            )
        })?;
        self.platform
            .write_state_file(&self.file, &encoded)
            .map_err(|error| (ExecError::from(error), false))?;
        self.platform
            .sync_directory(&self.directory)
            .map_err(|error| (ExecError::from(error), false))?;
        self.frames = frames;
        Ok(())
    }
}

fn progress_of(intent: &[u8]) -> (u64, [u8; 16]) {
    let mut cursor = [0u8; 8];
    cursor.copy_from_slice(&intent[..8]);
    let mut digest = [0u8; 16];
    digest.copy_from_slice(&intent[8..24]);
    (u64::from_le_bytes(cursor), digest)
}

fn preimage(region: Region, before: &[u8], after: &[u8]) -> Preimage {
    let mut hasher = Hasher::new();
    hasher.update(before);
    Preimage {
        offset: region.start(),
        len: region.len(),
        digest: hasher.finish(),
        data: before.to_vec(),
        after: after.to_vec(),
    }
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
