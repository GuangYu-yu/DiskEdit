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
                host.platform
                    .wipe(device, &ranges)
                    .map_err(|error| ExecError::Io(format!("擦除旧签名失败: {error}")))?;
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
            let content = content
                .ok_or_else(|| ExecError::InvalidPlan("内容属性步骤缺少内容类型".to_owned()))?;
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
            "文件系统执行器收到非文件系统步骤".to_owned(),
        )),
    }
}

fn declared_region(regions: &[Region]) -> Result<DeviceRegion, ExecError> {
    match regions {
        [region] => Ok(DeviceRegion::new(region.start(), region.len())),
        _ => Err(ExecError::InvalidPlan(
            "文件系统步骤必须声明恰好一个分区区间".to_owned(),
        )),
    }
}

/// 在分区设备视图上执行闭包；无论成功失败都释放本次挂接的 loop。
fn with_device<F>(
    host: &Host<'_>,
    partition: u32,
    region: DeviceRegion,
    work: F,
) -> Result<(), ExecError>
where
    F: FnOnce(&Path) -> Result<(), ExecError>,
{
    let device: PartitionDevice = host
        .platform
        .partition_device(host.target, partition, region)
        .map_err(|error| ExecError::Unsupported(format!("无法定位分区设备: {error}")))?;
    let outcome = work(&device.path);
    if let Some(loop_device) = &device.loop_device {
        if let Err(error) = host.platform.loop_detach(loop_device) {
            return match outcome {
                Ok(()) => Err(ExecError::Io(format!(
                    "文件系统操作完成但释放 {} 失败: {error}",
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
        .map_err(|error| ExecError::Unsupported(format!("{program:?} 不可用: {error}")))
}

fn run_tool(host: &Host<'_>, program: Program, args: &[&str]) -> Result<(), ExecError> {
    let output = host
        .platform
        .run(program, args)
        .map_err(|error| ExecError::Io(format!("运行 {program:?} 失败: {error}")))?;
    if output.succeeded() {
        Ok(())
    } else {
        Err(ExecError::Io(format!(
            "{program:?} 以 {:?} 结束: {}",
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
        | FsKind::Udf => Err(ExecError::Unsupported(format!("{kind:?} 不支持原地扩容"))),
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
                ExecError::Unsupported(format!("读取 btrfs 设备信息失败: {error}"))
            })?;
            if show.devices > 1 {
                return Err(ExecError::Unsupported("多设备 btrfs 不支持收缩".to_owned()));
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
        | FsKind::Udf => Err(ExecError::Unsupported(format!("{kind:?} 不支持收缩"))),
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
        .map_err(|error| ExecError::Io(format!("运行 E2fsck 失败: {error}")))?;
    // ext 检查器的退出位是修复结果之和：0 无问题，1 已修复，均表示可继续。
    match output.exit {
        ExitKind::Succeeded | ExitKind::Failed(1) => Ok(()),
        other @ (ExitKind::Failed(_) | ExitKind::Signaled(_) | ExitKind::Unknown) => {
            Err(ExecError::Io(format!(
                "E2fsck 以 {other:?} 结束: {}",
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
        .map_err(|error| ExecError::Unsupported(format!("无法建立挂载点: {error}")))?;
    let mounted = host
        .platform
        .mount(device, &point, Some(fstype), &[])
        .map_err(|error| ExecError::Io(format!("挂载 {} 失败: {error}", device.display())));
    if let Err(error) = mounted {
        drop(host.platform.remove_mount_point(&point));
        return Err(error);
    }
    let text = point.to_string_lossy().into_owned();
    let outcome = work(&text);
    let unmounted = host
        .platform
        .umount(&point)
        .map_err(|error| ExecError::Io(format!("卸载 {} 失败: {error}", point.display())));
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
        .map_err(|error| ExecError::Postcondition(format!("回读文件系统尺寸失败: {error}")))?;
    let satisfied = match direction {
        Growth::Grow => facts.bytes >= requested && facts.bytes <= region.len,
        Growth::Shrink => facts.bytes <= requested,
    };
    if satisfied {
        Ok(())
    } else {
        Err(ExecError::Postcondition(format!(
            "文件系统尺寸 {} 不满足请求 {}",
            facts.bytes, requested
        )))
    }
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
                "LVM 物理卷不由本工具的建卷动作创建".to_owned(),
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
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::InvalidPlan("内容属性步骤属性不符".to_owned()))
            }
        },
        ContentKind::Filesystem(FsKind::Xfs) => match property {
            SetProperty::Label | SetProperty::Uuid => Ok(Program::XfsAdmin),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::InvalidPlan("内容属性步骤属性不符".to_owned()))
            }
        },
        ContentKind::Filesystem(FsKind::Btrfs) => match property {
            SetProperty::Label => Ok(Program::Btrfs),
            SetProperty::Uuid => Ok(Program::Btrfstune),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::InvalidPlan("内容属性步骤属性不符".to_owned()))
            }
        },
        ContentKind::Filesystem(FsKind::Ntfs) => match property {
            SetProperty::Label => Ok(Program::Ntfslabel),
            SetProperty::Uuid if value == "random" => Ok(Program::Ntfslabel),
            SetProperty::Uuid => Err(ExecError::Unsupported(
                "ntfs 只支持生成新的串号，不能指定具体值".to_owned(),
            )),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::InvalidPlan("内容属性步骤属性不符".to_owned()))
            }
        },
        ContentKind::Filesystem(FsKind::Vfat) => match property {
            SetProperty::Label => Ok(Program::Fatlabel),
            SetProperty::Uuid | SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::Unsupported("vfat 只支持设置标签".to_owned()))
            }
        },
        ContentKind::Filesystem(FsKind::Exfat) => match property {
            SetProperty::Label => Ok(Program::Exfatlabel),
            SetProperty::Uuid | SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::Unsupported("exfat 只支持设置标签".to_owned()))
            }
        },
        ContentKind::Swap => match property {
            SetProperty::Label => Ok(Program::Swaplabel),
            SetProperty::Uuid if value != "random" => Ok(Program::Swaplabel),
            SetProperty::Uuid => Err(ExecError::Unsupported(
                "swap 只支持指定 UUID，不能生成新值".to_owned(),
            )),
            SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
                Err(ExecError::InvalidPlan("内容属性步骤属性不符".to_owned()))
            }
        },
        ContentKind::Filesystem(_)
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => Err(ExecError::Unsupported(
            "该内容不支持原地属性写入".to_owned(),
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
        .map_err(|error| ExecError::Postcondition(format!("读取内容属性失败: {error}")))?;
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
            "内容属性回读为 {observed:?}，与请求 {value:?} 不符"
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
                    "建卷后未读到 FAT 引导扇区签名".to_owned(),
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
                    "建卷后未读到交换区签名".to_owned(),
                ))
            }
        }
        ContentKind::Filesystem(_)
        | ContentKind::Lvm2Pv
        | ContentKind::Luks
        | ContentKind::BitLocker
        | ContentKind::Unknown => {
            let (offset, expected) = magic(content)
                .ok_or_else(|| ExecError::Unsupported("该内容没有可核对的签名".to_owned()))?;
            let bytes = channel.read_exact(region.offset + offset, expected.len())?;
            if bytes == expected {
                Ok(())
            } else {
                Err(ExecError::Postcondition(format!(
                    "建卷后签名不符：期望 {expected:?}，读回 {bytes:?}"
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
