//! 文件系统步骤执行：建卷、扩缩、检查与内容属性。
//!
//! 外部工具在分区设备视图上运行；步骤进入本模块前，原像帧已经落盘且步骤已标为
//! 不可逆，因此失败按"目标可能已改变"处理。每一步在工具退出后都回读目标字节
//! 或尺寸，不以工具的退出码代替核对。

use std::path::Path;

use diskedit_core::fs::{ContentKind, FsKind, MkfsKind, Program};
use diskedit_core::region::Region;

use super::{Context, ExecError, Host, WriteChannel};
use crate::app::decide::step::StepKind;
use crate::app::execute::token::Recorded;
use crate::app::observe::SetProperty;
use crate::platform::{ExitKind, FsSize, PartitionDevice, Region as DeviceRegion};

/// 建卷前擦除旧签名的单段长度。
const SIGNATURE_CHUNK: u64 = 512 * 1024;

pub fn apply(
    kind: &StepKind,
    regions: &[Region],
    channel: &mut WriteChannel<'_>,
    _token: &Recorded,
    _ctx: &Context<'_>,
    host: &Host<'_>,
) -> Result<(), ExecError> {
    let region = declared_region(regions)?;
    match kind {
        StepKind::MakeFilesystem {
            partition,
            filesystem,
            label,
        } => {
            let plan = mkfs_plan(*filesystem, label.as_deref())?;
            require(host, plan.program)?;
            let ranges = signature_ranges(region.len);
            with_device(host, *partition, region, |device| {
                let args = plan.arguments(device);
                host.platform.wipe(device, &ranges).map_err(|error| {
                    ExecError::Io(format!("failed to wipe old signatures: {error}"))
                })?;
                run_owned_tool(host, plan.program, &args)?;
                channel.barrier()?;
                verify_magic(channel, region, filesystem.content())?;
                Ok(())
            })
        }
        StepKind::GrowFilesystem {
            partition,
            filesystem,
            bytes,
        } => {
            let target = *bytes;
            with_device(host, *partition, region, |device| {
                grow(host, device, *filesystem)?;
                channel.barrier()?;
                verify_size(
                    host,
                    device,
                    region,
                    *filesystem,
                    target,
                    Growth::Grow,
                    channel,
                )
            })
        }
        StepKind::ShrinkFilesystem {
            partition,
            filesystem,
            bytes,
        } => {
            let target = *bytes;
            with_device(host, *partition, region, |device| {
                shrink(host, device, *filesystem, target)?;
                channel.barrier()?;
                verify_size(
                    host,
                    device,
                    region,
                    *filesystem,
                    target,
                    Growth::Shrink,
                    channel,
                )
            })
        }
        StepKind::SetProperty {
            partition,
            property,
            value,
            content,
        } => {
            let content = content.ok_or_else(|| {
                ExecError::InvalidPlan("content property step is missing content type".to_owned())
            })?;
            let program = attr_program(content, *property, value)?;
            require(host, program)?;
            require(host, Program::Blkid)?;
            with_device(host, *partition, region, |device| {
                let args = attr_arguments(*property, value, content, device);
                run_owned_tool(host, program, &args)?;
                channel.barrier()?;
                verify_attr(host, channel, device, *property, value)
            })
        }
        StepKind::GrowFilesystemOnline { .. } => Err(ExecError::InvalidPlan(
            "online filesystem steps are executed by the online path".to_owned(),
        )),
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::WriteTable { .. }
        | StepKind::AddPartition { .. }
        | StepKind::DeletePartition { .. }
        | StepKind::ResizePartition { .. }
        | StepKind::CopyPartition { .. }
        | StepKind::MovePartition { .. }
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords { .. } => Err(ExecError::InvalidPlan(
            "filesystem executor received a non-filesystem step".to_owned(),
        )),
    }
}

fn declared_region(regions: &[Region]) -> Result<DeviceRegion, ExecError> {
    match regions {
        [region] => Ok(DeviceRegion::new(region.start(), region.len())),
        _ => Err(ExecError::InvalidPlan(
            "filesystem step must declare exactly one partition region".to_owned(),
        )),
    }
}

/// 在分区设备视图上执行闭包；无论成功失败都释放本次挂接的 loop。
pub(crate) fn with_device<T, F>(
    host: &Host<'_>,
    partition: u32,
    region: DeviceRegion,
    work: F,
) -> Result<T, ExecError>
where
    F: FnOnce(&Path) -> Result<T, ExecError>,
{
    let device: PartitionDevice = host
        .platform
        .partition_device(host.target, partition, region)
        .map_err(|error| {
            ExecError::Unsupported(format!("cannot locate partition device: {error}"))
        })?;
    let outcome = work(&device.path);
    if let Some(loop_device) = &device.loop_device {
        if let Err(error) = host.platform.loop_detach(loop_device) {
            return match outcome {
                Ok(_) => Err(ExecError::Io(format!(
                    "filesystem operation completed but failed to release {}: {error}",
                    loop_device.display()
                ))),
                Err(failure) => Err(failure),
            };
        }
    }
    outcome
}

fn require(host: &Host<'_>, program: Program) -> Result<(), ExecError> {
    host.platform
        .require(program)
        .map(|_| ())
        .map_err(|error| ExecError::Unsupported(format!("{program:?} unavailable: {error}")))
}

fn run_tool(host: &Host<'_>, program: Program, args: &[&str]) -> Result<(), ExecError> {
    let output = host
        .platform
        .run(program, args)
        .map_err(|error| ExecError::Io(format!("failed to run {program:?}: {error}")))?;
    if output.succeeded() {
        Ok(())
    } else {
        Err(ExecError::Io(format!(
            "{program:?} exited with {:?}: {}",
            output.exit,
            output.stderr_text().trim()
        )))
    }
}

fn run_owned_tool(host: &Host<'_>, program: Program, args: &[String]) -> Result<(), ExecError> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_tool(host, program, &borrowed)
}

/// 扩展文件系统。
fn grow(host: &Host<'_>, device: &Path, kind: FsKind) -> Result<(), ExecError> {
    let text = device.to_string_lossy().into_owned();
    match kind {
        FsKind::Ext => {
            repair_ext(host, &text)?;
            run_tool(host, Program::Resize2fs, &[&text])
        }
        FsKind::Ntfs => ntfs_resize(host, &text, &["--force", "--force", text.as_str()]),
        FsKind::Xfs => mounted(host, device, "xfs", |point| {
            run_tool(host, Program::XfsGrowfs, &[point])
        }),
        FsKind::Btrfs => mounted(host, device, "btrfs", |point| {
            run_tool(
                host,
                Program::Btrfs,
                &["filesystem", "resize", "max", point],
            )
        }),
        FsKind::F2fs => run_tool(host, Program::ResizeF2fs, &[&text]),
        FsKind::Vfat => run_tool(host, Program::Fatresize, &["-s", "max", &text]),
        FsKind::Exfat
        | FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => Err(ExecError::Unsupported(format!(
            "{kind:?} does not support in-place growth"
        ))),
    }
}

/// 收缩文件系统。
fn shrink(host: &Host<'_>, device: &Path, kind: FsKind, bytes: u64) -> Result<(), ExecError> {
    let text = device.to_string_lossy().into_owned();
    match kind {
        FsKind::Ext => {
            repair_ext(host, &text)?;
            let sectors = format!("{}s", bytes / 512);
            run_tool(host, Program::Resize2fs, &[&text, &sectors])
        }
        FsKind::Ntfs => {
            let size = format!("{bytes}");
            ntfs_resize(
                host,
                &text,
                &["--size", size.as_str(), "--force", "--force", text.as_str()],
            )
        }
        FsKind::Btrfs => {
            let show = host.platform.btrfs_show(device).map_err(|error| {
                ExecError::Unsupported(format!("failed to read btrfs device info: {error}"))
            })?;
            if show.devices > 1 {
                return Err(ExecError::Unsupported(
                    "multi-device btrfs does not support shrink".to_owned(),
                ));
            }
            let size = format!("{bytes}");
            mounted(host, device, "btrfs", |point| {
                run_tool(
                    host,
                    Program::Btrfs,
                    &["filesystem", "resize", &size, point],
                )
            })
        }
        FsKind::Xfs
        | FsKind::F2fs
        | FsKind::Vfat
        | FsKind::Exfat
        | FsKind::Squashfs
        | FsKind::Erofs
        | FsKind::HfsPlus
        | FsKind::Apfs
        | FsKind::Bcachefs
        | FsKind::Jfs
        | FsKind::Reiserfs
        | FsKind::Udf => Err(ExecError::Unsupported(format!(
            "{kind:?} does not support shrink"
        ))),
    }
}

/// 修好再试一次：卷不干净时缩放会被拒绝，先修复再重试。
fn ntfs_resize(host: &Host<'_>, device: &str, args: &[&str]) -> Result<(), ExecError> {
    match run_tool(host, Program::Ntfsresize, args) {
        Ok(()) => Ok(()),
        Err(first) => {
            if run_tool(host, Program::Ntfsfix, &[device]).is_err() {
                return Err(first);
            }
            run_tool(host, Program::Ntfsresize, args).map_err(|_| first)
        }
    }
}

fn repair_ext(host: &Host<'_>, device: &str) -> Result<(), ExecError> {
    let output = host
        .platform
        .run(Program::E2fsck, &["-fp", device])
        .map_err(|error| ExecError::Io(format!("failed to run E2fsck: {error}")))?;
    // ext 检查器的退出位是修复结果之和：0 无问题，1 已修复，均表示可继续。
    match output.exit {
        ExitKind::Succeeded | ExitKind::Failed(1) => Ok(()),
        other @ (ExitKind::Failed(_) | ExitKind::Signaled(_) | ExitKind::Unknown) => {
            Err(ExecError::Io(format!(
                "E2fsck exited with {other:?}: {}",
                output.stderr_text().trim()
            )))
        }
    }
}

/// 需要挂载的操作：挂到私有临时目录，工具返回后卸载并清理。
fn mounted<F>(host: &Host<'_>, device: &Path, fstype: &str, work: F) -> Result<(), ExecError>
where
    F: FnOnce(&str) -> Result<(), ExecError>,
{
    let point = host
        .platform
        .make_mount_point()
        .map_err(|error| ExecError::Unsupported(format!("cannot create mount point: {error}")))?;
    let mounted = host
        .platform
        .mount(device, &point, Some(fstype), &[])
        .map_err(|error| ExecError::Io(format!("failed to mount {}: {error}", device.display())));
    if let Err(error) = mounted {
        drop(host.platform.remove_mount_point(&point));
        return Err(error);
    }
    let text = point.to_string_lossy().into_owned();
    let outcome = work(&text);
    let unmounted = host
        .platform
        .umount(&point)
        .map_err(|error| ExecError::Io(format!("failed to unmount {}: {error}", point.display())));
    drop(host.platform.remove_mount_point(&point));
    match (outcome, unmounted) {
        (Err(failure), _) => Err(failure),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

enum Growth {
    Grow,
    Shrink,
}

/// 工具退出后回读文件系统尺寸；不以退出码代替结果。
///
/// ext、ntfs、btrfs 的当前尺寸可被工具读出，按请求核对；其余族没有可靠的尺寸
/// 读数，退回到"签名仍完整"这一可观察事实，不把读不到写成已确认。
fn verify_size(
    host: &Host<'_>,
    device: &Path,
    region: DeviceRegion,
    kind: FsKind,
    requested: u64,
    direction: Growth,
    channel: &WriteChannel<'_>,
) -> Result<(), ExecError> {
    let trusted = matches!(kind, FsKind::Ext | FsKind::Ntfs | FsKind::Btrfs);
    if !trusted {
        return verify_magic(channel, region, ContentKind::Filesystem(kind));
    }
    let facts: FsSize = host
        .platform
        .filesystem_size_of(device, region.len, kind)
        .map_err(|error| {
            ExecError::Postcondition(format!("failed to read back filesystem size: {error}"))
        })?;
    let satisfied = match direction {
        Growth::Grow => reaches_end(facts.bytes, facts.unit, region.len),
        Growth::Shrink => facts.bytes <= requested,
    };
    if satisfied {
        Ok(())
    } else {
        Err(ExecError::Postcondition(format!(
            "filesystem size {} does not meet the request {}",
            facts.bytes, requested
        )))
    }
}

/// 扩容后是否已吃到区间末端。
///
/// 外部工具按自己的放置单元取整，末段装不下一个完整单元时会整段留空；因此
/// 判据是"末端落在最后一个单元内"（`bytes <= 末端 < bytes + unit`），而不是
/// 字节相等。ext 在 1.47.x 上会把 900 MiB 卷止于 896 MiB，就是这个形态。
pub(crate) fn reaches_end(bytes: u64, unit: u64, limit: u64) -> bool {
    bytes <= limit
        && bytes
            .checked_add(unit.max(1))
            .is_some_and(|next| limit < next)
}

/// 建卷参数：程序、固定选项与标签形式。
struct MkfsPlan {
    program: Program,
    prefix: Vec<String>,
    label_flag: &'static str,
    label: Option<String>,
}

impl MkfsPlan {
    fn arguments(&self, device: &Path) -> Vec<String> {
        let mut args = self.prefix.clone();
        if let Some(label) = &self.label {
            args.push(self.label_flag.to_owned());
            args.push(label.clone());
        }
        args.push(device.to_string_lossy().into_owned());
        args
    }
}

fn mkfs_plan(kind: MkfsKind, label: Option<&str>) -> Result<MkfsPlan, ExecError> {
    let (program, prefix, label_flag) = match kind {
        MkfsKind::Ext2 | MkfsKind::Ext3 | MkfsKind::Ext4 => (
            Program::Mke2fs,
            vec!["-t".to_owned(), kind.as_str().to_owned()],
            "-L",
        ),
        MkfsKind::Xfs => (Program::MkfsXfs, vec!["-f".to_owned()], "-L"),
        MkfsKind::Btrfs => (Program::MkfsBtrfs, vec!["-f".to_owned()], "-L"),
        MkfsKind::F2fs => (Program::MkfsF2fs, vec!["-f".to_owned()], "-l"),
        MkfsKind::Vfat => (Program::MkfsVfat, Vec::new(), "-n"),
        MkfsKind::Exfat => (Program::MkfsExfat, Vec::new(), "-n"),
        MkfsKind::Ntfs => (Program::MkfsNtfs, Vec::new(), "-L"),
        MkfsKind::Swap => (Program::Mkswap, Vec::new(), "-L"),
        MkfsKind::Lvm2Pv => {
            return Err(ExecError::Unsupported(
                "LVM physical volumes are not created by this tool's mkfs action".to_owned(),
            ));
        }
    };
    Ok(MkfsPlan {
        program,
        prefix,
        label_flag,
        label: label.map(str::to_owned),
    })
}

fn signature_ranges(device_length: u64) -> Vec<DeviceRegion> {
    let mut starts = vec![0u64, 2 * 1024 * 1024, 4 * 1024 * 1024];
    if device_length > SIGNATURE_CHUNK {
        starts.push(device_length.saturating_sub(SIGNATURE_CHUNK));
    }
    let mut ranges = Vec::new();
    for start in starts {
        if start >= device_length {
            continue;
        }
        let len = SIGNATURE_CHUNK.min(device_length - start);
        ranges.push(DeviceRegion::new(start, len));
    }
    ranges
}

fn attr_program(
    content: ContentKind,
    property: SetProperty,
    value: &str,
) -> Result<Program, ExecError> {
    match content {
        ContentKind::Filesystem(FsKind::Ext) => match property {
            SetProperty::Label => Ok(Program::Tune2fs),
            SetProperty::Uuid => Ok(Program::Tune2fs),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::InvalidPlan("content property step property mismatch".to_owned()),
            ),
        },
        ContentKind::Filesystem(FsKind::Xfs) => match property {
            SetProperty::Label | SetProperty::Uuid => Ok(Program::XfsAdmin),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::InvalidPlan("content property step property mismatch".to_owned()),
            ),
        },
        ContentKind::Filesystem(FsKind::Btrfs) => match property {
            SetProperty::Label => Ok(Program::Btrfs),
            SetProperty::Uuid => Ok(Program::Btrfstune),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::InvalidPlan("content property step property mismatch".to_owned()),
            ),
        },
        ContentKind::Filesystem(FsKind::Ntfs) => match property {
            SetProperty::Label => Ok(Program::Ntfslabel),
            SetProperty::Uuid if value == "random" => Ok(Program::Ntfslabel),
            SetProperty::Uuid => Err(ExecError::Unsupported(
                "ntfs only supports generating a new serial; cannot set an explicit value"
                    .to_owned(),
            )),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::InvalidPlan("content property step property mismatch".to_owned()),
            ),
        },
        ContentKind::Filesystem(FsKind::Vfat) => match property {
            SetProperty::Label => Ok(Program::Fatlabel),
            SetProperty::Uuid | SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::Unsupported("vfat only supports setting the label".to_owned()),
            ),
        },
        ContentKind::Filesystem(FsKind::Exfat) => match property {
            SetProperty::Label => Ok(Program::Exfatlabel),
            SetProperty::Uuid | SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::Unsupported("exfat only supports setting the label".to_owned()),
            ),
        },
        ContentKind::Swap => match property {
            SetProperty::Label => Ok(Program::Swaplabel),
            SetProperty::Uuid if value != "random" => Ok(Program::Swaplabel),
            SetProperty::Uuid => Err(ExecError::Unsupported(
                "swap only supports an explicit UUID; cannot generate a new one".to_owned(),
            )),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => Err(
                ExecError::InvalidPlan("content property step property mismatch".to_owned()),
            ),
        },
        ContentKind::Filesystem(_)
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => Err(ExecError::Unsupported(
            "in-place property writes unsupported for this content".to_owned(),
        )),
    }
}

fn attr_arguments(
    property: SetProperty,
    value: &str,
    content: ContentKind,
    device: &Path,
) -> Vec<String> {
    let device = device.to_string_lossy().into_owned();
    let uuid_random = value == "random";
    match content {
        ContentKind::Filesystem(FsKind::Ext) => {
            let flag = if property == SetProperty::Uuid {
                "-U"
            } else {
                "-L"
            };
            let text = if uuid_random { "random" } else { value };
            vec![flag.to_owned(), text.to_owned(), device]
        }
        ContentKind::Filesystem(FsKind::Xfs) => {
            let flag = if property == SetProperty::Uuid {
                "-U"
            } else {
                "-L"
            };
            let text = if uuid_random { "generate" } else { value };
            vec![flag.to_owned(), text.to_owned(), device]
        }
        ContentKind::Filesystem(FsKind::Btrfs) => {
            if property == SetProperty::Uuid {
                if uuid_random {
                    vec!["-u".to_owned(), device]
                } else {
                    vec!["-f".to_owned(), "-U".to_owned(), value.to_owned(), device]
                }
            } else {
                vec![
                    "filesystem".to_owned(),
                    "label".to_owned(),
                    device,
                    value.to_owned(),
                ]
            }
        }
        ContentKind::Filesystem(FsKind::Ntfs) => {
            if property == SetProperty::Uuid {
                vec!["--new-serial".to_owned(), device]
            } else {
                vec![device, value.to_owned()]
            }
        }
        ContentKind::Filesystem(FsKind::Vfat) | ContentKind::Filesystem(FsKind::Exfat) => {
            vec![device, value.to_owned()]
        }
        ContentKind::Swap => {
            let flag = if property == SetProperty::Uuid {
                "-U"
            } else {
                "-L"
            };
            vec![flag.to_owned(), value.to_owned(), device]
        }
        ContentKind::Filesystem(_)
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => vec![device, value.to_owned()],
    }
}

fn verify_attr(
    host: &Host<'_>,
    channel: &WriteChannel<'_>,
    device: &Path,
    property: SetProperty,
    value: &str,
) -> Result<(), ExecError> {
    let field = if property == SetProperty::Uuid {
        "UUID"
    } else {
        "LABEL"
    };
    let text = device.to_string_lossy().into_owned();
    let output = host
        .platform
        .run(Program::Blkid, &["-p", "-o", "value", "-s", field, &text])
        .map_err(|error| {
            ExecError::Postcondition(format!("failed to read content property: {error}"))
        })?;
    let _ = channel;
    let observed = output.stdout_text().trim().to_owned();
    let satisfied = if property == SetProperty::Uuid && value == "random" {
        is_random_uuid(&observed)
    } else {
        observed == value.trim()
    };
    if satisfied {
        Ok(())
    } else {
        Err(ExecError::Postcondition(format!(
            "content property read back as {observed:?}, mismatch with requested {value:?}"
        )))
    }
}

fn is_random_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

/// 建卷后核对目标上的族签名；读不到或不符即后置条件失败。
fn verify_magic(
    channel: &WriteChannel<'_>,
    region: DeviceRegion,
    content: ContentKind,
) -> Result<(), ExecError> {
    match content {
        ContentKind::Filesystem(FsKind::Vfat) => {
            let boot = channel.read_exact(region.offset, 512)?;
            let fat = boot[54..57] == *b"FAT" || boot[82..85] == *b"FAT";
            let signature = boot[510] == 0x55 && boot[511] == 0xaa;
            if fat && signature {
                Ok(())
            } else {
                Err(ExecError::Postcondition(
                    "FAT boot sector signature not found after mkfs".to_owned(),
                ))
            }
        }
        ContentKind::Swap => {
            let take = usize::try_from(region.len.min(65_536)).unwrap_or(65_536);
            let head = channel.read_exact(region.offset, take)?;
            let found = [4086usize, 65_526]
                .into_iter()
                .any(|at| head.get(at..at + 10) == Some(b"SWAPSPACE2"));
            if found {
                Ok(())
            } else {
                Err(ExecError::Postcondition(
                    "swap signature not found after mkfs".to_owned(),
                ))
            }
        }
        ContentKind::Filesystem(_)
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => {
            let (offset, expected) = magic(content).ok_or_else(|| {
                ExecError::Unsupported("no verifiable signature for this content".to_owned())
            })?;
            let bytes = channel.read_exact(region.offset + offset, expected.len())?;
            if bytes == expected {
                Ok(())
            } else {
                Err(ExecError::Postcondition(format!(
                    "signature mismatch after mkfs: expected {expected:?}, read back {bytes:?}"
                )))
            }
        }
    }
}

fn magic(content: ContentKind) -> Option<(u64, &'static [u8])> {
    Some(match content {
        ContentKind::Filesystem(FsKind::Ext) => (1080, &[0x53, 0xef]),
        ContentKind::Filesystem(FsKind::Xfs) => (0, b"XFSB"),
        ContentKind::Filesystem(FsKind::Btrfs) => (65_600, b"_BHRfS_M"),
        ContentKind::Filesystem(FsKind::F2fs) => (1024, &[0x10, 0x20, 0xf5, 0xf2]),
        ContentKind::Filesystem(FsKind::Ntfs) => (3, b"NTFS    "),
        ContentKind::Filesystem(FsKind::Exfat) => (3, b"EXFAT   "),
        ContentKind::Filesystem(_)
        | ContentKind::Swap
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_ranges_cover_head_and_tail() {
        let ranges = signature_ranges(16 * 1024 * 1024);
        assert!(ranges.iter().any(|range| range.offset == 0));
        assert!(ranges.iter().any(|range| range.offset == 2 * 1024 * 1024));
        assert!(ranges.iter().any(|range| range.offset == 4 * 1024 * 1024));
        assert!(ranges.iter().all(|range| range.end() <= 16 * 1024 * 1024));
        assert!(ranges.iter().any(|range| range.end() == 16 * 1024 * 1024));
    }

    #[test]
    fn signature_ranges_stay_inside_small_devices() {
        let ranges = signature_ranges(256 * 1024);
        assert_eq!(ranges, vec![DeviceRegion::new(0, 256 * 1024)]);
    }

    #[test]
    fn growth_reaches_end_within_one_unit() {
        let mib = 1024 * 1024;
        // 900 MiB 卷止于 896 MiB：相差 4 MiB，落在一个 128 MiB 块组内。
        assert!(reaches_end(896 * mib, 128 * mib, 900 * mib));
        // 末端正好相等也是到位。
        assert!(reaches_end(900 * mib, 128 * mib, 900 * mib));
        // 差出整整一个单元即视为没扩到位。
        assert!(!reaches_end(772 * mib, 128 * mib, 900 * mib));
        // 越过区间末端不成立。
        assert!(!reaches_end(901 * mib, 128 * mib, 900 * mib));
    }

    #[test]
    fn random_uuid_recognises_canonical_form() {
        assert!(is_random_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!is_random_uuid("not-a-uuid"));
        assert!(!is_random_uuid("123e4567e89b12d3a456426614174000"));
    }

    #[test]
    fn mkfs_plan_puts_label_before_device() {
        let plan = mkfs_plan(MkfsKind::Ext4, Some("data")).unwrap();
        let args = plan.arguments(Path::new("partition-device"));
        assert_eq!(args[0], "-t");
        assert_eq!(args[1], "ext4");
        assert_eq!(args[2], "-L");
        assert_eq!(args[3], "data");
        assert_eq!(args[4], "partition-device");
    }
}
