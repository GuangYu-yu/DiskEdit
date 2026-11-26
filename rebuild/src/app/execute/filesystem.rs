//! 文件系统步骤的执行：经外部工具改写分区内容。
//!
//! 这类步骤没有原像、不可回滚，因此先落一条不可逆意图记录，再动目标字节；
//! 工具退出或中断之后，记录保留在原处交给恢复流程。镜像经 loop 绑定成块设备，
//! 整盘经分区节点寻址，上层只看"设备路径"。

use std::path::{Path, PathBuf};

use diskedit_core::fs::{FsKind, MkfsKind, Program};
use diskedit_core::region::Region;

use super::transaction::RecordSession;
use super::{ExecError, FailureKind, Outcome, Recorded, WriteChannel};
use crate::app::decide::{ConfirmedPlan, Step, StepKind};
use crate::platform::{LoopBinding, Platform, TargetKind};

/// 意图记录里的固定标签：一次文件系统改写。
const FS_INTENT: &[u8] = b"filesystem";
/// 残留签名的擦除半径。
const ERASE_RADIUS: u64 = 32 << 20;
/// 擦除点的上限。
const ERASE_COUNT: u64 = 512;
/// 每个擦除点覆盖的字节数。
const SIGNATURE_SITE_BYTES: u64 = 512 << 10;
/// 分区末尾保护的擦除窗口。
const ERASE_WINDOW_BYTES: u64 = 1 << 30;
/// 单次擦除写入的字节数。
const WIPE_CHUNK: usize = 1 << 20;

/// 执行一份只含文件系统步骤的计划。
pub fn run(platform: &dyn Platform, target: &Path, plan: &ConfirmedPlan) -> Outcome {
    match execute(platform, target, plan) {
        Ok(()) => Outcome::completed(),
        Err((error, touched)) => failure(error, touched),
    }
}

/// 一步文件系统动作。
enum FsAction {
    Make {
        partition: u32,
        filesystem: MkfsKind,
        label: Option<String>,
    },
    Grow {
        partition: u32,
        filesystem: FsKind,
    },
    Shrink {
        partition: u32,
    },
}

impl FsAction {
    fn partition(&self) -> u32 {
        match self {
            Self::Make { partition, .. }
            | Self::Grow { partition, .. }
            | Self::Shrink { partition } => *partition,
        }
    }
}

fn execute(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<(), (ExecError, bool)> {
    let (action, region) = fs_action(plan.steps()).map_err(|error| (error, false))?;
    // 锁前核对一次身份，持锁后再核对一次。
    check_identity(platform, target, plan).map_err(|error| (error, false))?;
    let mut session =
        RecordSession::prepare(platform, plan.identity()).map_err(|error| (error, false))?;
    check_identity(platform, target, plan).map_err(|error| (error, false))?;
    session
        .begin_irreversible(plan.request_digest(), FS_INTENT)
        .map_err(|error| (error, false))?;

    let device = resolve_device(
        platform,
        target,
        action.partition(),
        region.start(),
        region.len(),
    )
    .map_err(|error| (error, false))?;

    match &action {
        FsAction::Make {
            filesystem, label, ..
        } => {
            let sector_size =
                target_sector_size(platform, target).map_err(|error| (error, false))?;
            let ranges = erase_ranges(region.len(), sector_size)
                .map_err(|why| (ExecError::InvalidPlan(why), false))?;
            {
                let mut handle = platform
                    .open_target(device.device())
                    .map_err(|error| (ExecError::from(error), false))?;
                let token = session.token();
                let declared = [Region::sized(0, region.len()).map_err(|_| {
                    (
                        ExecError::InvalidPlan(
                            "the partition region is not constructible".to_owned(),
                        ),
                        false,
                    )
                })?];
                let mut channel = WriteChannel::new(handle.as_mut(), &declared);
                wipe(&mut channel, &token, &ranges).map_err(|error| (error, true))?;
                channel.barrier().map_err(|error| (error, true))?;
            }
            let (program, mut args) =
                mkfs_invocation(*filesystem, label.as_deref()).ok_or_else(|| {
                    (
                        ExecError::InvalidPlan(
                            "this content kind has no volume to create".to_owned(),
                        ),
                        true,
                    )
                })?;
            args.push(device.device().display().to_string());
            run_checked(platform, program, &args).map_err(|(error, _)| (error, true))?;
        }
        FsAction::Grow { filesystem, .. } => {
            grow(platform, *filesystem, device.device())?;
        }
        FsAction::Shrink { .. } => {
            return Err((
                ExecError::InvalidPlan("shrink is not part of this executor".to_owned()),
                false,
            ));
        }
    }

    session.commit().map_err(|error| (error, true))?;
    Ok(())
}

fn check_identity(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<(), ExecError> {
    let observed = platform.identity(target).map_err(ExecError::from)?;
    if observed.matches(plan.identity()) {
        Ok(())
    } else {
        Err(ExecError::IdentityChanged(
            "the target is no longer the object the plan was made for".to_owned(),
        ))
    }
}

/// 计划里的文件系统步骤与它声明的区间。
fn fs_action(steps: &[Step]) -> Result<(FsAction, Region), ExecError> {
    for step in steps {
        let action = match &step.kind {
            StepKind::MakeFilesystem {
                partition,
                filesystem,
                label,
            } => Some(FsAction::Make {
                partition: *partition,
                filesystem: *filesystem,
                label: label.clone(),
            }),
            StepKind::GrowFilesystem {
                partition,
                filesystem,
                ..
            } => Some(FsAction::Grow {
                partition: *partition,
                filesystem: *filesystem,
            }),
            StepKind::ShrinkFilesystem { partition, .. } => Some(FsAction::Shrink {
                partition: *partition,
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
            | StepKind::GrowFilesystemOnline { .. }
            | StepKind::CopyPartition { .. }
            | StepKind::MovePartition { .. }
            | StepKind::RefreshKernelView
            | StepKind::RecoverRecords { .. } => None,
        };
        if let Some(action) = action {
            let region = step.regions.first().copied().ok_or_else(|| {
                ExecError::InvalidPlan("the filesystem step declares no region".to_owned())
            })?;
            return Ok((action, region));
        }
    }
    Err(ExecError::InvalidPlan(
        "the plan has no filesystem step".to_owned(),
    ))
}

/// 一步文件系统动作对应的块设备视图。
struct Device {
    binding: Option<LoopBinding>,
    path: Option<PathBuf>,
}

impl Device {
    fn device(&self) -> &Path {
        match (&self.binding, &self.path) {
            (Some(binding), _) => binding.device(),
            (None, Some(path)) => path,
            (None, None) => Path::new(""),
        }
    }
}

fn resolve_device(
    platform: &dyn Platform,
    target: &Path,
    partition: u32,
    base: u64,
    len: u64,
) -> Result<Device, ExecError> {
    match platform.path_kind(target) {
        TargetKind::File => Ok(Device {
            binding: Some(
                platform
                    .bind_loop(target, base, len)
                    .map_err(ExecError::from)?,
            ),
            path: None,
        }),
        TargetKind::BlockDevice => Ok(Device {
            binding: None,
            path: Some(
                platform
                    .partition_device(target, partition)
                    .map_err(ExecError::from)?,
            ),
        }),
        TargetKind::Other => Err(ExecError::InvalidPlan(
            "the target is neither an image nor a block device".to_owned(),
        )),
    }
}

fn target_sector_size(platform: &dyn Platform, target: &Path) -> Result<u64, ExecError> {
    let handle = platform
        .open_target_read_only(target)
        .map_err(ExecError::from)?;
    Ok(handle.sector_size().unwrap_or(512))
}

/// 建卷所需的外部程序与固定参数；设备路径由调用方追加。
fn mkfs_invocation(kind: MkfsKind, label: Option<&str>) -> Option<(Program, Vec<String>)> {
    let (program, force, ext, label_flag) = match kind {
        MkfsKind::Ext2 => (Program::Mke2fs, false, Some("ext2"), "-L"),
        MkfsKind::Ext3 => (Program::Mke2fs, false, Some("ext3"), "-L"),
        MkfsKind::Ext4 => (Program::Mke2fs, false, Some("ext4"), "-L"),
        MkfsKind::Xfs => (Program::MkfsXfs, true, None, "-L"),
        MkfsKind::Btrfs => (Program::MkfsBtrfs, true, None, "-L"),
        MkfsKind::F2fs => (Program::MkfsF2fs, true, None, "-l"),
        MkfsKind::Vfat => (Program::MkfsVfat, false, None, "-n"),
        MkfsKind::Exfat => (Program::MkfsExfat, false, None, "-L"),
        MkfsKind::Ntfs => (Program::MkfsNtfs, false, None, "-L"),
        MkfsKind::Swap => (Program::Mkswap, false, None, "-L"),
        MkfsKind::Lvm2Pv => return None,
    };
    let mut args = Vec::new();
    if force {
        args.push("-f".to_owned());
    }
    if let Some(ext) = ext {
        args.push("-t".to_owned());
        args.push(ext.to_owned());
    }
    if let Some(label) = label {
        args.push(label_flag.to_owned());
        args.push(label.to_owned());
    }
    Some((program, args))
}

/// 扩容的固定程序序列；挂载点形态另加临时挂载。
enum GrowPlan {
    InPlace(Vec<(Program, Vec<String>)>),
    Mounted(Vec<(Program, Vec<String>)>),
}

fn grow_plan(kind: FsKind) -> Option<GrowPlan> {
    let ops = match kind {
        FsKind::Ext => vec![
            (Program::E2fsck, vec!["-fp".to_owned()]),
            (Program::Resize2fs, Vec::new()),
            (Program::Dumpe2fs, Vec::new()),
        ],
        FsKind::Ntfs => vec![(Program::Ntfsresize, vec!["-f".to_owned(), "-f".to_owned()])],
        FsKind::F2fs => vec![
            (Program::FsckF2fs, Vec::new()),
            (Program::ResizeF2fs, Vec::new()),
        ],
        FsKind::Vfat => vec![(Program::Fatresize, vec!["-s".to_owned(), "max".to_owned()])],
        FsKind::Xfs => return Some(GrowPlan::Mounted(vec![(Program::XfsGrowfs, Vec::new())])),
        FsKind::Btrfs => {
            return Some(GrowPlan::Mounted(vec![(
                Program::Btrfs,
                vec![
                    "filesystem".to_owned(),
                    "resize".to_owned(),
                    "max".to_owned(),
                ],
            )]));
        }
        FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::Exfat
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => return None,
    };
    Some(GrowPlan::InPlace(ops))
}

fn grow(platform: &dyn Platform, kind: FsKind, device: &Path) -> Result<(), (ExecError, bool)> {
    let plan = grow_plan(kind).ok_or_else(|| {
        (
            ExecError::InvalidPlan("this filesystem has no grow program".to_owned()),
            false,
        )
    })?;
    match plan {
        GrowPlan::InPlace(ops) => {
            let mut touched = false;
            for (program, mut args) in ops {
                args.push(device.display().to_string());
                match run_checked(platform, program, &args) {
                    Ok(()) => touched = true,
                    Err((error, spawned)) => {
                        return Err((error, touched || spawned));
                    }
                }
            }
            Ok(())
        }
        GrowPlan::Mounted(ops) => grow_mounted(platform, device, &ops),
    }
}

fn grow_mounted(
    platform: &dyn Platform,
    device: &Path,
    ops: &[(Program, Vec<String>)],
) -> Result<(), (ExecError, bool)> {
    let mount_point = platform
        .temporary_directory("mount")
        .map_err(|error| (ExecError::from(error), false))?;
    let mount_args = [
        device.display().to_string(),
        mount_point.display().to_string(),
    ];
    let mounted = platform.run(Program::Mount, &mount_args);
    let mount_error = match mounted {
        Ok(run) if run.code == Some(0) => None,
        Ok(run) => Some(ExecError::Postcondition(format!(
            "mount exited {}: {}",
            code_text(run.code),
            run.stderr.trim()
        ))),
        Err(error) => Some(ExecError::from(error)),
    };
    if let Some(error) = mount_error {
        platform.remove_directory(&mount_point).ok();
        return Err((error, false));
    }

    let mut touched = false;
    let mut failure = None;
    for (program, args) in ops {
        let mut args = args.clone();
        args.push(mount_point.display().to_string());
        match run_checked(platform, *program, &args) {
            Ok(()) => touched = true,
            Err((error, spawned)) => {
                touched |= spawned;
                failure = Some(error);
                break;
            }
        }
    }

    let unmount = platform.run(Program::Umount, &[mount_point.display().to_string()]);
    let unmount_error = match unmount {
        Ok(run) if run.code == Some(0) => None,
        Ok(run) => Some(ExecError::Postcondition(format!(
            "umount exited {}: {}",
            code_text(run.code),
            run.stderr.trim()
        ))),
        Err(error) => Some(ExecError::from(error)),
    };
    if unmount_error.is_none() {
        platform.remove_directory(&mount_point).ok();
    }
    if let Some(error) = failure {
        return Err((error, touched));
    }
    if let Some(error) = unmount_error {
        return Err((error, touched));
    }
    Ok(())
}

fn run_checked(
    platform: &dyn Platform,
    program: Program,
    args: &[String],
) -> Result<(), (ExecError, bool)> {
    match platform.run(program, args) {
        Ok(run) if run.code == Some(0) => Ok(()),
        Ok(run) => Err((
            ExecError::Postcondition(format!(
                "{} exited {}: {}",
                platform.tool_name(program),
                code_text(run.code),
                run.stderr.trim()
            )),
            true,
        )),
        Err(error) => Err((ExecError::from(error), false)),
    }
}

fn code_text(code: Option<i32>) -> String {
    code.map(|code| code.to_string())
        .unwrap_or_else(|| "by a signal".to_owned())
}

/// 经唯一写通道把擦除区间的字节清零并落盘。
fn wipe(
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ranges: &[(u64, u64)],
) -> Result<(), ExecError> {
    let zeros = vec![0u8; WIPE_CHUNK];
    for &(offset, len) in ranges {
        let mut at = 0;
        while at < len {
            let take = (len - at).min(WIPE_CHUNK as u64);
            let region = Region::sized(offset + at, take).map_err(|_| {
                ExecError::InvalidPlan("an erase range does not fit the partition".to_owned())
            })?;
            channel.write(token, region, &zeros[..take as usize])?;
            at += take;
        }
        channel.barrier()?;
    }
    Ok(())
}

/// 建卷前要擦掉的残留签名区间；起点与长度按扇区对齐，末点保护分区末尾。
fn erase_ranges(part_len: u64, sector_size: u64) -> Result<Vec<(u64, u64)>, String> {
    if part_len == 0 {
        return Err("the partition holds no bytes to format".to_owned());
    }
    let unit = sector_size.max(1);
    let count = ERASE_COUNT.min(part_len / ERASE_RADIUS);
    let site = ceil_to_unit(SIGNATURE_SITE_BYTES, unit).min(part_len);
    let mut ranges = Vec::new();
    for index in 0..count {
        let offset = floor_to_unit(part_len.saturating_mul(index) / count, unit);
        let len = site.min(part_len - offset);
        if len > 0 {
            ranges.push((offset, len));
        }
    }
    let window = ceil_to_unit(ERASE_WINDOW_BYTES, unit);
    let tail_start = floor_to_unit(part_len.saturating_sub(window), unit);
    let tail_len = part_len - tail_start;
    if !ranges
        .iter()
        .any(|&(offset, len)| offset == tail_start && len == tail_len)
    {
        ranges.push((tail_start, tail_len));
    }
    Ok(ranges)
}

fn floor_to_unit(value: u64, unit: u64) -> u64 {
    value / unit * unit
}

fn ceil_to_unit(value: u64, unit: u64) -> u64 {
    value.div_ceil(unit).saturating_mul(unit)
}

fn failure(error: ExecError, touched: bool) -> Outcome {
    if touched {
        return Outcome::needs_recovery(error.to_string());
    }
    match &error {
        ExecError::Unsupported(_) => {
            Outcome::unwritten(FailureKind::Unsupported, error.to_string())
        }
        ExecError::Record(_) | ExecError::IdentityChanged(_) | ExecError::InvalidPlan(_) => {
            Outcome::unwritten(FailureKind::Refused, error.to_string())
        }
        ExecError::Platform(_) | ExecError::Io(_) => {
            Outcome::unwritten(FailureKind::FailedBeforeWrite, error.to_string())
        }
        ExecError::Postcondition(_) | ExecError::Injected(_) => {
            Outcome::unwritten(FailureKind::Internal, error.to_string())
        }
    }
}

// covers: B15
#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(kind: MkfsKind, label: Option<&str>) -> (Program, Vec<String>) {
        mkfs_invocation(kind, label).expect("家庭有建卷程序")
    }

    #[test]
    fn mkfs_arguments_follow_the_family() {
        assert_eq!(
            args_of(MkfsKind::Ext4, None),
            (Program::Mke2fs, vec!["-t".to_owned(), "ext4".to_owned()])
        );
        assert_eq!(
            args_of(MkfsKind::Xfs, None),
            (Program::MkfsXfs, vec!["-f".to_owned()])
        );
        assert_eq!(
            args_of(MkfsKind::Btrfs, None),
            (Program::MkfsBtrfs, vec!["-f".to_owned()])
        );
        assert_eq!(
            args_of(MkfsKind::F2fs, Some("data")),
            (
                Program::MkfsF2fs,
                vec!["-f".to_owned(), "-l".to_owned(), "data".to_owned()]
            )
        );
        assert_eq!(
            args_of(MkfsKind::Vfat, Some("BOOT")),
            (Program::MkfsVfat, vec!["-n".to_owned(), "BOOT".to_owned()])
        );
    }

    #[test]
    fn lvm_mkfs_has_no_tool() {
        assert!(mkfs_invocation(MkfsKind::Lvm2Pv, None).is_none());
    }

    #[test]
    fn erase_ranges_protect_both_ends() {
        let ranges = erase_ranges(64 << 20, 512).expect("区间");
        assert_eq!(ranges.first().map(|range| range.0), Some(0));
        assert_eq!(ranges.last().map(|range| range.0 + range.1), Some(64 << 20));
        for &(offset, len) in &ranges {
            assert_eq!(offset % 512, 0);
            assert!(offset + len <= 64 << 20);
        }
    }

    #[test]
    fn grow_programs_are_fixed_per_family() {
        let ext = grow_plan(FsKind::Ext).expect("ext 扩容");
        match ext {
            GrowPlan::InPlace(ops) => {
                assert_eq!(ops[0].0, Program::E2fsck);
                assert_eq!(ops[0].1, vec!["-fp".to_owned()]);
                assert_eq!(ops[1].0, Program::Resize2fs);
            }
            GrowPlan::Mounted(_) => panic!("ext 不挂载扩容"),
        }
        match grow_plan(FsKind::Btrfs).expect("btrfs 扩容") {
            GrowPlan::Mounted(ops) => {
                assert_eq!(ops[0].0, Program::Btrfs);
                assert_eq!(
                    ops[0].1,
                    vec![
                        "filesystem".to_owned(),
                        "resize".to_owned(),
                        "max".to_owned()
                    ]
                );
            }
            GrowPlan::InPlace(_) => panic!("btrfs 经挂载扩容"),
        }
        assert!(grow_plan(FsKind::Squashfs).is_none());
    }

    #[test]
    fn erase_window_never_exceeds_the_partition() {
        let ranges = erase_ranges(4096, 512).expect("区间");
        assert_eq!(ranges, vec![(0, 4096)]);
        assert!(erase_ranges(0, 512).is_err());
    }

    #[test]
    fn units_round_as_expected() {
        assert_eq!(floor_to_unit(4097, 512), 4096);
        assert_eq!(ceil_to_unit(4097, 512), 4608);
        assert_eq!(SIGNATURE_SITE_BYTES % 1024, 0);
    }
}
