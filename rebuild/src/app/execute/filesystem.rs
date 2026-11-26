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
use crate::platform::{LoopBinding, Platform, TargetKind, ToolRun};

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
    match execute_plan(platform, target, plan) {
        Ok(()) => Outcome::completed(),
        Err((error, touched)) => failure(error, touched),
    }
}

/// 执行一份只含在线扩容步骤的计划。
///
/// 在线形态直接在用户已有的挂载上运行工具，不写分区表、不登记原像、不卸载挂载。
/// 工具一旦启动，目标字节就可能已经改变，失败按"已写、待恢复"报告。
pub fn run_online(platform: &dyn Platform, plan: &ConfirmedPlan) -> Outcome {
    match execute_online(platform, plan) {
        Ok(()) => Outcome::completed(),
        Err((error, started)) => {
            if started {
                Outcome::needs_recovery(error.to_string())
            } else {
                failure(error, false)
            }
        }
    }
}

/// 在线扩容的现场事实。
struct Online<'a> {
    mount_point: &'a Path,
    device: &'a Path,
    disk: &'a Path,
    partition: u32,
    filesystem: FsKind,
    bytes: u64,
}

fn execute_online(platform: &dyn Platform, plan: &ConfirmedPlan) -> Result<(), (ExecError, bool)> {
    let step = single_online_step(plan.steps()).map_err(|error| (error, false))?;
    let StepKind::GrowFilesystemOnline {
        mount_point,
        device,
        disk,
        partition,
        filesystem,
        bytes,
    } = &step.kind
    else {
        return Err((
            ExecError::InvalidPlan("the online plan holds no online step".to_owned()),
            false,
        ));
    };
    let online = Online {
        mount_point,
        device,
        disk,
        partition: *partition,
        filesystem: *filesystem,
        bytes: *bytes,
    };
    confirm_mount(platform, &online).map_err(|error| (error, false))?;
    check_identity(platform, online.disk, plan).map_err(|error| (error, false))?;

    if let Some((program, args)) = online_invocation(&online) {
        if platform.tool_available(program) {
            return run_online_native(platform, &online, program, &args);
        }
    }
    run_online_carrier(platform, &online)
}

/// 计划里必须恰好有一个在线步骤。
fn single_online_step(steps: &[Step]) -> Result<&Step, ExecError> {
    let mut found = steps
        .iter()
        .filter(|step| matches!(step.kind, StepKind::GrowFilesystemOnline { .. }));
    let step = found
        .next()
        .ok_or_else(|| ExecError::InvalidPlan("the plan has no online step".to_owned()))?;
    if steps.len() != 1 || found.next().is_some() {
        return Err(ExecError::InvalidPlan(
            "an online plan must contain exactly one online step".to_owned(),
        ));
    }
    Ok(step)
}

/// 挂载点必须仍指向计划确认时的同一个分区。
fn confirm_mount(platform: &dyn Platform, online: &Online<'_>) -> Result<(), ExecError> {
    use crate::platform::MountFacts;
    match platform
        .mount_facts(online.mount_point)
        .map_err(ExecError::from)?
    {
        MountFacts::Mounted {
            device,
            disk,
            partition,
            ..
        } if device == online.device && disk == online.disk && partition == online.partition => {
            Ok(())
        }
        MountFacts::Mounted { .. }
        | MountFacts::NotMounted
        | MountFacts::NotPartition { .. }
        | MountFacts::Ambiguous
        | MountFacts::Unmapped { .. } => Err(ExecError::IdentityChanged(
            "the mount point no longer resolves to the confirmed partition".to_owned(),
        )),
    }
}

/// 在线扩容的程序与参数；没有在线工具的族返回 `None`。
fn online_invocation(online: &Online<'_>) -> Option<(Program, Vec<String>)> {
    let mount = online.mount_point.display().to_string();
    let device = online.device.display().to_string();
    match online.filesystem {
        FsKind::Ext => Some((Program::Resize2fs, vec![device])),
        FsKind::Xfs => Some((Program::XfsGrowfs, vec![mount])),
        FsKind::Btrfs => Some((
            Program::Btrfs,
            vec![
                "filesystem".to_owned(),
                "resize".to_owned(),
                "max".to_owned(),
                mount,
            ],
        )),
        FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::F2fs
        | FsKind::Exfat
        | FsKind::Vfat
        | FsKind::Ntfs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => None,
    }
}

/// 本机执行在线扩容：工具运行一次，随后回读尺寸确认真的长到分区末尾。
fn run_online_native(
    platform: &dyn Platform,
    online: &Online<'_>,
    program: Program,
    args: &[String],
) -> Result<(), (ExecError, bool)> {
    match platform.run(program, args) {
        Err(error) => Err((ExecError::from(error), false)),
        Ok(run) => match run.code {
            Some(0) => verify_online_size(platform, online),
            Some(_) => Err((
                ExecError::Postcondition(format!(
                    "{} exited {}: {}",
                    platform.tool_name(program),
                    code_text(run.code),
                    run.stderr.trim()
                )),
                true,
            )),
            None => Err((
                ExecError::Postcondition(format!(
                    "{} was terminated by a signal; its result is unknown",
                    platform.tool_name(program)
                )),
                true,
            )),
        },
    }
}

/// 在线扩容之后回读：ext 与 btrfs 必须长到分区末尾；其余族没有可用的只读探针时
/// 按"已写、待恢复"报告，而不是凭工具退出码当作成功。
fn verify_online_size(
    platform: &dyn Platform,
    online: &Online<'_>,
) -> Result<(), (ExecError, bool)> {
    let device = online.device.display().to_string();
    match online.filesystem {
        FsKind::Ext => {
            let run = platform
                .run(Program::Dumpe2fs, &["-h".to_owned(), device])
                .map_err(|error| (ExecError::from(error), true))?;
            ensure_probe_ran(platform, Program::Dumpe2fs, &run)?;
            verify_ext_size(&run, online.bytes).map_err(|error| (error, true))
        }
        // btrfs 的超级块直接给出总字节数与分配单元，不需要挂载状态；`-f` 是只读的
        // 全量输出。`total_bytes` 覆盖到区间末端（落在最后一个 sectorsize 内）才算
        // 长到位——工具退 0 而超级块仍是旧尺寸时不能报成功。
        FsKind::Btrfs => {
            let run = platform
                .run(
                    Program::Btrfs,
                    &[
                        "inspect-internal".to_owned(),
                        "dump-super".to_owned(),
                        "-f".to_owned(),
                        device,
                    ],
                )
                .map_err(|error| (ExecError::from(error), true))?;
            ensure_probe_ran(platform, Program::Btrfs, &run)?;
            verify_btrfs_size(&run, online.bytes).map_err(|error| (error, true))
        }
        FsKind::Xfs
        | FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::F2fs
        | FsKind::Exfat
        | FsKind::Vfat
        | FsKind::Ntfs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => Err((
            ExecError::Postcondition(
                "no read-back probe is available for this filesystem after the online grow"
                    .to_owned(),
            ),
            true,
        )),
    }
}

/// 只读探针本身失败：文件系统可能已被工具改过，按"已写、待恢复"报告。
fn ensure_probe_ran(
    platform: &dyn Platform,
    program: Program,
    run: &ToolRun,
) -> Result<(), (ExecError, bool)> {
    if run.code == Some(0) {
        return Ok(());
    }
    Err((
        ExecError::Postcondition(format!(
            "{} exited {}: {}",
            platform.tool_name(program),
            code_text(run.code),
            run.stderr.trim()
        )),
        true,
    ))
}

/// 本机没有在线工具时交给虚拟机载体；没有载体就按"不支持"拒绝，而不是照常尝试。
fn run_online_carrier(
    platform: &dyn Platform,
    online: &Online<'_>,
) -> Result<(), (ExecError, bool)> {
    use crate::platform::{OnlineRequest, PlatformError};
    let carrier = platform
        .carrier()
        .map_err(|error| (ExecError::from(error), false))?;
    let Some(carrier) = carrier else {
        return Err((
            ExecError::Unsupported(format!(
                "no online grow tool for {} is available and no VM carrier is configured; install \
                 the filesystem tools or set DISKEDIT_VM_BUNDLE",
                online.filesystem.as_str()
            )),
            false,
        ));
    };
    let sector_size = target_sector_size(platform, online.disk).unwrap_or(512);
    let request = OnlineRequest {
        mount_point: online.mount_point.to_path_buf(),
        device: online.device.to_path_buf(),
        disk: online.disk.to_path_buf(),
        partition: online.partition,
        filesystem: online.filesystem,
        bytes: online.bytes,
        sector_size,
    };
    match carrier.online_grow(&request) {
        Ok(report) => match (report.blocks, report.block_size) {
            (Some(blocks), Some(block_size)) => {
                let total = blocks.saturating_mul(block_size);
                if total > online.bytes || online.bytes - total >= block_size {
                    return Err((
                        ExecError::Postcondition(format!(
                            "the guest reports a {total}-byte filesystem, not the expected {} bytes",
                            online.bytes
                        )),
                        true,
                    ));
                }
                Ok(())
            }
            _ => Err((
                ExecError::Postcondition(
                    "the guest gave no filesystem geometry after the online grow".to_owned(),
                ),
                true,
            )),
        },
        Err(PlatformError::NotSupported(why)) => Err((ExecError::Unsupported(why), false)),
        // 来宾已经启动过：目标可能已被改动，按"已写、待恢复"报告。
        Err(error) => Err((ExecError::Postcondition(error.to_string()), true)),
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
        /// 文件系统应长到的字节数（分区容量）。
        bytes: u64,
    },
    Shrink {
        partition: u32,
        filesystem: FsKind,
        bytes: u64,
    },
}

impl FsAction {
    fn partition(&self) -> u32 {
        match self {
            Self::Make { partition, .. }
            | Self::Grow { partition, .. }
            | Self::Shrink { partition, .. } => *partition,
        }
    }
}

pub(super) fn execute_plan(
    platform: &dyn Platform,
    target: &Path,
    plan: &ConfirmedPlan,
) -> Result<(), (ExecError, bool)> {
    let (action, region) = fs_action(plan.steps()).map_err(|error| (error, false))?;
    ensure_block_capability(platform, target).map_err(|error| (error, false))?;
    // 锁前核对一次身份，持锁后再核对一次。
    check_identity(platform, target, plan).map_err(|error| (error, false))?;
    let mut session =
        RecordSession::prepare(platform, plan.identity()).map_err(|error| (error, false))?;
    check_identity(platform, target, plan).map_err(|error| (error, false))?;
    session
        .begin_irreversible(plan.request_digest(), FS_INTENT)
        .map_err(|error| (error, false))?;
    // 本步原像已落盘；此后每一次工具调用都必须带着它，写类工具无法在记录之外执行。
    let token = session.token();

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
            run_checked(platform, program, &args, &token)
                .map(|_| ())
                .map_err(|(error, _)| (error, true))?;
        }
        FsAction::Grow {
            filesystem, bytes, ..
        } => {
            grow(platform, *filesystem, device.device(), *bytes, &token)?;
        }
        FsAction::Shrink {
            filesystem, bytes, ..
        } => {
            shrink(platform, *filesystem, device.device(), *bytes, &token)?;
        }
    }

    session.commit().map_err(|error| (error, true))?;
    Ok(())
}

/// 块设备目标需要块层能力（分区视图、内核重读、挂载）。
///
/// 普通文件经 loop 得到分区视图，不受限；块设备目标在缺能力且无载体时按
/// "不支持"拒绝，而不是照常执行再得到内核 `EPERM`。
pub(super) fn ensure_block_capability(
    platform: &dyn Platform,
    target: &Path,
) -> Result<(), ExecError> {
    match capability_refusal(
        platform.path_kind(target),
        platform.filesystem_ready(target),
    ) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 纯判据：文件目标不需要块层能力；块设备目标按探测结果决定。
fn capability_refusal(kind: TargetKind, ready: bool) -> Option<ExecError> {
    if kind == TargetKind::BlockDevice && !ready {
        return Some(ExecError::Unsupported(
            "this host cannot adjust a block device's partition view (missing CAP_SYS_ADMIN) and \
             no VM carrier is configured; make DISKEDIT_VM_BUNDLE available or run on a host with \
             the capability"
                .to_owned(),
        ));
    }
    None
}

pub(super) fn check_identity(
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
                bytes,
            } => Some(FsAction::Grow {
                partition: *partition,
                filesystem: *filesystem,
                bytes: *bytes,
            }),
            StepKind::ShrinkFilesystem {
                partition,
                filesystem,
                bytes,
            } => Some(FsAction::Shrink {
                partition: *partition,
                filesystem: *filesystem,
                bytes: *bytes,
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
pub(super) struct Device {
    binding: Option<LoopBinding>,
    path: Option<PathBuf>,
}

impl Device {
    pub(super) fn device(&self) -> &Path {
        match (&self.binding, &self.path) {
            (Some(binding), _) => binding.device(),
            (None, Some(path)) => path,
            (None, None) => Path::new(""),
        }
    }
}

pub(super) fn resolve_device(
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

pub(super) fn target_sector_size(platform: &dyn Platform, target: &Path) -> Result<u64, ExecError> {
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

pub(super) fn grow(
    platform: &dyn Platform,
    kind: FsKind,
    device: &Path,
    bytes: u64,
    token: &Recorded,
) -> Result<(), (ExecError, bool)> {
    let plan = grow_plan(kind).ok_or_else(|| {
        (
            ExecError::InvalidPlan("this filesystem has no grow program".to_owned()),
            false,
        )
    })?;
    match plan {
        GrowPlan::InPlace(ops) => {
            let mut touched = false;
            let mut last = None;
            for (program, mut args) in ops {
                args.push(device.display().to_string());
                match run_checked(platform, program, &args, token) {
                    Ok(run) => {
                        touched = true;
                        last = Some((program, run));
                    }
                    Err((error, spawned)) => {
                        return Err((error, touched || spawned));
                    }
                }
            }
            // 扩到分区容量之后回读几何：工具报成功不等于文件系统真的长满了。
            if kind == FsKind::Ext
                && let Some((Program::Dumpe2fs, run)) = last
            {
                verify_ext_size(&run, bytes).map_err(|error| (error, touched))?;
            }
            Ok(())
        }
        GrowPlan::Mounted(ops) => grow_mounted(platform, device, &ops, token),
    }
}

/// 收缩文件系统到指定字节数；只覆盖可离线收缩的族。
pub(super) fn shrink(
    platform: &dyn Platform,
    kind: FsKind,
    device: &Path,
    bytes: u64,
    token: &Recorded,
) -> Result<(), (ExecError, bool)> {
    let dev = device.display().to_string();
    match kind {
        FsKind::Ext => {
            // 尺寸后缀 `s` 以 512 字节扇区计。
            let sectors = bytes / 512;
            let last = run_sequence(
                platform,
                &[
                    (Program::E2fsck, vec!["-fp".to_owned(), dev.clone()]),
                    (Program::Resize2fs, vec![dev.clone(), format!("{sectors}s")]),
                    (Program::Dumpe2fs, vec![dev.clone()]),
                ],
                token,
            )?;
            if let Some((Program::Dumpe2fs, run)) = last {
                verify_ext_size(&run, bytes).map_err(|error| (error, true))?;
            }
            Ok(())
        }
        FsKind::Ntfs => run_sequence(
            platform,
            &[(
                Program::Ntfsresize,
                vec![
                    "-f".to_owned(),
                    "--size".to_owned(),
                    bytes.to_string(),
                    dev.clone(),
                ],
            )],
            token,
        )
        .map(|_| ()),
        FsKind::Btrfs => grow_mounted(
            platform,
            device,
            &[(
                Program::Btrfs,
                vec![
                    "filesystem".to_owned(),
                    "resize".to_owned(),
                    bytes.to_string(),
                ],
            )],
            token,
        ),
        FsKind::Xfs
        | FsKind::F2fs
        | FsKind::Vfat
        | FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::Exfat
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => Err((
            ExecError::InvalidPlan("this filesystem has no offline shrink program".to_owned()),
            false,
        )),
    }
}

/// 顺序运行外部工具；每个调用都在写令牌包围之下。返回最后一次成功的运行，
/// 供调用方做尺寸回读。
fn run_sequence(
    platform: &dyn Platform,
    ops: &[(Program, Vec<String>)],
    token: &Recorded,
) -> Result<Option<(Program, ToolRun)>, (ExecError, bool)> {
    let mut touched = false;
    let mut last = None;
    for (program, args) in ops {
        match run_checked(platform, *program, args, token) {
            Ok(run) => {
                touched = true;
                last = Some((*program, run));
            }
            Err((error, spawned)) => return Err((error, touched || spawned)),
        }
    }
    Ok(last)
}

pub(super) fn grow_mounted(
    platform: &dyn Platform,
    device: &Path,
    ops: &[(Program, Vec<String>)],
    token: &Recorded,
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
        match run_checked(platform, *program, &args, token) {
            Ok(_) => touched = true,
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

/// 运行一个写类工具；令牌是"本步原像已落盘"的唯一凭据，调用点无法跳过登记。
///
/// 文件系统检查工具的退出码 1/2 表示"检查出问题并已修复"（例如脏 ext），那是成功而不是
/// 失败；修复本身已写目标，后续步骤若失败，调用方按"已写过"归入部分完成。
pub(super) fn run_checked(
    platform: &dyn Platform,
    program: Program,
    args: &[String],
    token: &Recorded,
) -> Result<ToolRun, (ExecError, bool)> {
    let _ = token;
    match run_outcome(program, platform.run(program, args)) {
        ToolOutcome::Done(run) => Ok(run),
        // 进程跑过、结局已知或不可知：目标可能已被改动，进恢复流程。
        ToolOutcome::Failed(run) | ToolOutcome::Indeterminate(run) => Err((
            ExecError::Postcondition(format!(
                "{} exited {}: {}",
                platform.tool_name(program),
                code_text(run.code),
                run.stderr.trim()
            )),
            true,
        )),
        // 进程根本没启动：目标一个字节都没动。
        ToolOutcome::NotStarted(error) => Err((ExecError::from(error), false)),
    }
}

/// 外部工具一次运行的可观察结局。
///
/// 关键是"没跑起来"与"跑过但结局不可知"必须分开：前者可以直接报未改动，
/// 后者可能已经写了一半，只有它能进入恢复流程。
enum ToolOutcome {
    /// 启动并正常结束（检查器的"已修复"也算）。
    Done(ToolRun),
    /// 启动并退出非零且不是可接受的成功码。
    Failed(ToolRun),
    /// 没有启动，工具不可用或无法执行。
    NotStarted(crate::platform::PlatformError),
    /// 启动了但拿不到退出状态（被信号终止）。
    Indeterminate(ToolRun),
}

/// 该程序的退出码是否表示成功。检查器的 1 与 2 是"已修复/已修复需重启"，
/// 校验通过的命令在脏文件系统上会给出这两个码。
fn exit_is_clean(program: Program, code: i32) -> bool {
    if let Program::E2fsck = program {
        return (0..=2).contains(&code);
    }
    code == 0
}

fn run_outcome(program: Program, result: crate::platform::PlatformResult<ToolRun>) -> ToolOutcome {
    match result {
        Err(error) => ToolOutcome::NotStarted(error),
        Ok(run) => match run.code {
            Some(code) if exit_is_clean(program, code) => ToolOutcome::Done(run),
            Some(_) => ToolOutcome::Failed(run),
            None => ToolOutcome::Indeterminate(run),
        },
    }
}

/// 从文件系统尺寸探针的输出读块数与块大小。
fn ext_geometry(stdout: &str) -> Option<(u64, u64)> {
    let mut count = None;
    let mut size = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Block count:") {
            count = rest.split_whitespace().next().and_then(|t| t.parse().ok());
        } else if let Some(rest) = line.strip_prefix("Block size:") {
            size = rest.split_whitespace().next().and_then(|t| t.parse().ok());
        }
    }
    count.zip(size)
}

/// 回读核对：文件系统必须长到目标字节数（按块的整数倍对齐）。
fn verify_ext_size(run: &ToolRun, expected: u64) -> Result<(), ExecError> {
    let (count, size) = ext_geometry(&run.stdout).ok_or_else(|| {
        ExecError::Postcondition("the filesystem probe did not report the geometry".to_owned())
    })?;
    let total = count
        .checked_mul(size)
        .ok_or_else(|| ExecError::Postcondition("the filesystem geometry overflows".to_owned()))?;
    if total > expected || expected - total >= size {
        return Err(ExecError::Postcondition(format!(
            "the filesystem is {total} bytes, not the expected {expected} bytes rounded to its {size}-byte blocks"
        )));
    }
    Ok(())
}

/// btrfs 超级块里的 `total_bytes` 与 `sectorsize`：前者覆盖到区间末端、后者是放置单元。
/// `total_bytes` 必须落在 `[expected - sectorsize, expected]` 内；解析失败不猜默认值。
fn verify_btrfs_size(run: &ToolRun, expected: u64) -> Result<(), ExecError> {
    let total = leading_field(&run.stdout, "total_bytes").ok_or_else(|| {
        ExecError::Postcondition("the btrfs superblock probe did not report total_bytes".to_owned())
    })?;
    let unit = leading_field(&run.stdout, "sectorsize").ok_or_else(|| {
        ExecError::Postcondition("the btrfs superblock probe did not report sectorsize".to_owned())
    })?;
    let unit = unit.max(1);
    if total > expected || expected - total >= unit {
        return Err(ExecError::Postcondition(format!(
            "the filesystem is {total} bytes, not the expected {expected} bytes rounded to its {unit}-byte sectors"
        )));
    }
    Ok(())
}

/// `dump-super` 输出的行首字段：只接受行首的 `name` 加空白再加十进制数。
/// 不带行首锚定会同时命中 `dev_item.total_bytes` 与备份超级块的同名字段。
fn leading_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let first = rest.chars().next()?;
        if !first.is_ascii_whitespace() {
            return None;
        }
        rest.split_whitespace().next()?.parse().ok()
    })
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

    // covers: I9
    #[test]
    fn write_class_programs_are_named_and_bounded_by_the_token() {
        use diskedit_core::fs::Access;
        // 执行器只经 Program 身份调用外部工具。写类工具都带写令牌参数，且文件系统
        // 改写后以尺寸探针回读核对；只读探针不改变目标。
        assert_eq!(Program::Dumpe2fs.access(), Access::ReadOnly);
        for program in [Program::E2fsck, Program::Resize2fs, Program::Mke2fs] {
            assert_eq!(
                program.access(),
                Access::WritesTarget,
                "{program:?} 写目标，必须在写令牌包围下执行"
            );
        }
        let filled = ToolRun {
            code: Some(0),
            stdout: "Block count:   16\nBlock size:    512\n".to_owned(),
            stderr: String::new(),
        };
        assert!(verify_ext_size(&filled, 16 * 512).is_ok());
    }

    // covers: B27
    #[test]
    fn a_tool_without_an_exit_status_is_indeterminate() {
        use crate::platform::PlatformError;

        let done = ToolRun {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(matches!(
            run_outcome(Program::Resize2fs, Ok(done.clone())),
            ToolOutcome::Done(_)
        ));

        let failed = ToolRun {
            code: Some(1),
            ..done.clone()
        };
        assert!(matches!(
            run_outcome(Program::Resize2fs, Ok(failed)),
            ToolOutcome::Failed(_)
        ));

        let signalled = ToolRun { code: None, ..done };
        assert!(matches!(
            run_outcome(Program::Resize2fs, Ok(signalled)),
            ToolOutcome::Indeterminate(_)
        ));

        assert!(matches!(
            run_outcome(
                Program::Resize2fs,
                Err(PlatformError::not_supported("no tool"))
            ),
            ToolOutcome::NotStarted(_)
        ));
    }

    // covers: D4
    #[test]
    fn a_repaired_filesystem_is_a_success_and_a_later_failure_is_partial() {
        // 脏 ext 上检查器会报 1（已修复）；那是成功，修复本身已写目标。
        let repaired = ToolRun {
            code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(matches!(
            run_outcome(Program::E2fsck, Ok(repaired)),
            ToolOutcome::Done(_)
        ));
        // 4 表示存在未修复的错误：不得当作修复成功。
        let uncorrected = ToolRun {
            code: Some(4),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(matches!(
            run_outcome(Program::E2fsck, Ok(uncorrected)),
            ToolOutcome::Failed(_)
        ));
        assert!(exit_is_clean(Program::E2fsck, 2));
        assert!(!exit_is_clean(Program::Resize2fs, 1));
    }

    #[test]
    fn the_size_readback_accepts_only_a_filled_filesystem() {
        let filled = ToolRun {
            code: Some(0),
            stdout: "Block count:              12288\nBlock size:               4096\n".to_owned(),
            stderr: String::new(),
        };
        assert!(verify_ext_size(&filled, 12288 * 4096).is_ok());
        // 比目标小了一整块以上：没有长满。
        assert!(verify_ext_size(&filled, 12288 * 4096 + 4096).is_err());
        // 超出目标：不得接受。
        assert!(verify_ext_size(&filled, 12288 * 4096 - 4096).is_err());
        // 输出里没有几何：无法核对，按错误交回。
        let silent = ToolRun {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(verify_ext_size(&silent, 4096).is_err());
    }

    // covers: D11
    #[test]
    fn a_block_device_without_the_capability_is_refused_as_unsupported() {
        // 普通文件经 loop 得到分区视图，不需要块层能力。
        assert!(capability_refusal(TargetKind::File, false).is_none());
        // 块设备缺能力且无载体：按不支持拒绝，而不是照常执行再得到 EPERM。
        assert!(matches!(
            capability_refusal(TargetKind::BlockDevice, false),
            Some(ExecError::Unsupported(_))
        ));
        // 有能力时放行。
        assert!(capability_refusal(TargetKind::BlockDevice, true).is_none());
    }
}
