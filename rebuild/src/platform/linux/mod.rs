//! Linux 端口实现。
//!
//! 一切与内核接口、虚拟文件系统、外部程序路径有关的知识都收在本子树；
//! 上层只看见 [`crate::platform`] 里的中立类型与端口 trait。

pub mod carrier;
pub mod dm;
mod fs_size;
pub mod kernel;
pub mod loopdev;
pub mod lvm;
pub mod mount;
pub mod mount_location;
pub mod occupancy;
pub mod records;
pub mod source;
pub mod target;

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;

use crate::platform::{
    BtrfsShow, ExitKind, FsSize, Occupancy, Output, Partition, PartitionDevice, Platform,
    PlatformError, PlatformResult, PvFacts, Region, TargetHandle,
};

pub use crate::platform::{PlatformError as Error, PlatformResult as Result};

/// Linux 宿主端口。
#[derive(Debug, Default)]
pub struct Linux;

impl Linux {
    pub const fn new() -> Self {
        Self
    }

    fn open_target_read_only(path: &Path) -> PlatformResult<target::Target> {
        target::Target::open_read_only(path)
    }
}

impl Platform for Linux {
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(target::Target::open(path)?))
    }

    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(Self::open_target_read_only(path)?))
    }

    fn fingerprint(&self, path: &Path) -> PlatformResult<Fingerprint> {
        let target = Self::open_target_read_only(path)?;
        target::fingerprint(&target)
    }

    fn partitions(&self, path: &Path) -> PlatformResult<Vec<Partition>> {
        let target = Self::open_target_read_only(path)?;
        target::partitions(&target)
    }

    fn occupancy(&self, path: &Path) -> PlatformResult<Occupancy> {
        let target = Self::open_target_read_only(path)?;
        occupancy::probe(&target)
    }

    fn reread(&self, path: &Path) -> PlatformResult<()> {
        kernel::reread(path)
    }

    fn update_table(&self, path: &Path) -> PlatformResult<()> {
        match self.run(Program::Partx, &["-u", &path.to_string_lossy()]) {
            Ok(output) if output.succeeded() => return Ok(()),
            _ => {}
        }
        // 兜底：按盘上表逐分区 `BLKPG` 补建；读不到表时如实失败。
        let partitions = self.partitions(path)?;
        let ranges: Vec<(u32, u64, u64)> = partitions
            .iter()
            .map(|partition| {
                (
                    partition.number,
                    partition.start_lba.saturating_mul(512),
                    partition.sectors.saturating_mul(512),
                )
            })
            .collect();
        kernel::sync_partitions(path, &ranges)
    }

    fn btrfs_show(&self, path: &Path) -> PlatformResult<BtrfsShow> {
        let output = self.run(
            Program::Btrfs,
            &["filesystem", "show", "--raw", &path.to_string_lossy()],
        )?;
        if !output.succeeded() {
            return Err(PlatformError::tools(
                "btrfs",
                output.stderr_text().trim().to_string(),
            ));
        }
        let (fsid, devices) = kernel::parse_show(&output.stdout_text()).ok_or_else(|| {
            PlatformError::parse("btrfs filesystem show 输出中没有可解析的 fsid 或设备数")
        })?;
        Ok(BtrfsShow { fsid, devices })
    }

    fn pvs(&self, path: &Path) -> PlatformResult<Vec<PvFacts>> {
        let args = lvm::pvs_args(path);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(Program::Pvs, &borrowed)?;
        if !output.succeeded() {
            // 不是物理卷；pvs 以非零退出，这里按"没有 PV"处理。
            return Ok(Vec::new());
        }
        let rows = lvm::parse_pvs(&output.stdout_text())?;
        Ok(rows
            .into_iter()
            .map(|row| PvFacts {
                name: row.name,
                size_bytes: row.size_bytes,
                pe_start_bytes: row.pe_start_bytes,
                vg_name: row.vg_name,
            })
            .collect())
    }

    fn partition_device(
        &self,
        target: &Path,
        partition: u32,
        region: Region,
    ) -> PlatformResult<PartitionDevice> {
        let opened = Self::open_target_read_only(target)?;
        match opened.kind() {
            crate::platform::TargetKind::Block => Ok(PartitionDevice {
                path: target::partition_node(&opened, partition)?,
                loop_device: None,
            }),
            crate::platform::TargetKind::File => {
                let device = self.loop_attach(target, region.offset, region.len)?;
                Ok(PartitionDevice {
                    path: device.clone(),
                    loop_device: Some(device),
                })
            }
        }
    }

    fn filesystem_size(
        &self,
        target: &Path,
        partition: u32,
        region: Region,
        kind: diskedit_core::fs::FsKind,
    ) -> PlatformResult<FsSize> {
        let device = self.partition_device(target, partition, region)?;
        let result = self.probe_size(&device.path, region.len, kind);
        if let Some(loop_device) = &device.loop_device {
            drop(self.loop_detach(loop_device));
        }
        result
    }

    fn filesystem_size_of(
        &self,
        device: &Path,
        device_bytes: u64,
        kind: diskedit_core::fs::FsKind,
    ) -> PlatformResult<FsSize> {
        self.probe_size(device, device_bytes, kind)
    }

    fn wipe(&self, device: &Path, ranges: &[Region]) -> PlatformResult<()> {
        use std::io::{Seek, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(device)
            .map_err(|error| PlatformError::io(device, error))?;
        let metadata = file
            .metadata()
            .map_err(|error| PlatformError::io(device, error))?;
        let length = if metadata.file_type().is_block_device() {
            // 块设备的 `st_size` 恒为 0，容量只能向内核查询。
            source::BlockSource::open(device)?.len_bytes()
        } else {
            metadata.len()
        };
        let zeros = [0u8; 4096];
        for range in ranges {
            let end = range.end();
            if end > length {
                return Err(PlatformError::parse(format!(
                    "清零区间 {range:?} 越过设备长度 {length}"
                )));
            }
            let mut written = 0u64;
            while written < range.len {
                let chunk = usize::try_from((range.len - written).min(zeros.len() as u64))
                    .unwrap_or(zeros.len());
                file.seek(std::io::SeekFrom::Start(range.offset + written))
                    .map_err(|error| PlatformError::io(device, error))?;
                file.write_all(&zeros[..chunk])
                    .map_err(|error| PlatformError::io(device, error))?;
                written += chunk as u64;
            }
        }
        file.sync_data()
            .map_err(|error| PlatformError::io(device, error))
    }

    fn make_mount_point(&self) -> PlatformResult<PathBuf> {
        let base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or_default();
        let point = base.join(format!("diskedit.mnt.{}.{nanos:08x}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&point)
            .map_err(|error| PlatformError::io(&point, error))?;
        Ok(point)
    }

    fn remove_mount_point(&self, point: &Path) -> PlatformResult<()> {
        std::fs::remove_dir(point).map_err(|error| PlatformError::io(point, error))
    }

    fn loop_attach(&self, backing: &Path, offset: u64, size: u64) -> PlatformResult<PathBuf> {
        let args = loopdev::attach_args(backing, offset, size);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(Program::Losetup, &borrowed)?;
        if !output.succeeded() {
            return Err(PlatformError::tools(
                "losetup",
                output.stderr_text().trim().to_string(),
            ));
        }
        loopdev::parse_device(&output.stdout_text())
    }

    fn loop_detach(&self, device: &Path) -> PlatformResult<()> {
        let args = loopdev::detach_args(device);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(Program::Losetup, &borrowed)?;
        if output.succeeded() {
            Ok(())
        } else {
            Err(PlatformError::tools(
                "losetup",
                output.stderr_text().trim().to_string(),
            ))
        }
    }

    fn mount(
        &self,
        source: &Path,
        mount_point: &Path,
        fstype: Option<&str>,
        options: &[&str],
    ) -> PlatformResult<()> {
        let args = mount::mount_args(source, mount_point, fstype, options);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(Program::Mount, &borrowed)?;
        if output.succeeded() {
            Ok(())
        } else {
            Err(PlatformError::tools(
                "mount",
                output.stderr_text().trim().to_string(),
            ))
        }
    }

    fn umount(&self, mount_point: &Path) -> PlatformResult<()> {
        let args = mount::umount_args(mount_point);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(Program::Umount, &borrowed)?;
        if output.succeeded() {
            Ok(())
        } else {
            Err(PlatformError::tools(
                "umount",
                output.stderr_text().trim().to_string(),
            ))
        }
    }

    fn mount_location(&self, mount_point: &Path) -> PlatformResult<crate::platform::MountLocation> {
        mount_location::resolve(mount_point)
    }

    fn require(&self, program: Program) -> PlatformResult<PathBuf> {
        resolve(program)
    }

    fn records(
        &self,
        target: &Path,
        identity: Fingerprint,
    ) -> PlatformResult<Box<dyn crate::platform::RecordStore>> {
        let kind = target::Target::open_read_only(target)?.kind();
        Ok(Box::new(records::LinuxRecords::new(
            target,
            kind,
            &identity.bytes(),
        )))
    }

    fn record_dirs(&self, root: &Path) -> PlatformResult<Vec<crate::platform::RecordDir>> {
        records::record_dirs(root)
    }

    fn read_record(&self, dir: &Path, name: &str) -> PlatformResult<Vec<u8>> {
        records::read_entry(dir, name)
    }

    fn run(&self, program: Program, args: &[&str]) -> PlatformResult<Output> {
        let executable = resolve(program)?;
        let result = Command::new(&executable)
            .args(args)
            .output()
            .map_err(|error| {
                PlatformError::tools(
                    executable.display().to_string(),
                    format!("无法启动：{error}"),
                )
            })?;
        Ok(Output {
            exit: exit_kind(&result.status),
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }

    fn filesystem_ready(&self) -> bool {
        // 分区设备视图与块设备写入都要求特权；非特权宿主改走虚拟机载体。
        unsafe { libc::geteuid() == 0 }
    }

    fn carrier_available(&self) -> bool {
        true
    }

    fn carrier_target(&self) -> Option<&'static str> {
        Some(carrier::GUEST_TARGET)
    }

    fn launch_carrier(
        &self,
        launch: &crate::platform::CarrierLaunch,
    ) -> PlatformResult<Box<dyn crate::platform::CarrierProcess>> {
        carrier::launch(launch)
    }
}

fn exit_kind(status: &std::process::ExitStatus) -> ExitKind {
    if let Some(code) = status.code() {
        if code == 0 {
            ExitKind::Succeeded
        } else {
            ExitKind::Failed(code)
        }
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            match status.signal() {
                Some(signal) => ExitKind::Signaled(signal),
                None => ExitKind::Unknown,
            }
        }
        #[cfg(not(unix))]
        {
            ExitKind::Unknown
        }
    }
}

/// 在 `PATH` 中解析外部程序。
fn resolve(program: Program) -> PlatformResult<PathBuf> {
    let name = executable_name(program);
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(PlatformError::tools(name, "在 PATH 中找不到可执行文件"))
}

/// 程序身份到可执行名。可执行名只在本文件维护。
fn executable_name(program: Program) -> &'static str {
    match program {
        Program::E2fsck => "e2fsck",
        Program::Resize2fs => "resize2fs",
        Program::Dumpe2fs => "dumpe2fs",
        Program::Mke2fs => "mke2fs",
        Program::Tune2fs => "tune2fs",
        Program::Ntfsresize => "ntfsresize",
        Program::Ntfsfix => "ntfsfix",
        Program::Ntfslabel => "ntfslabel",
        Program::FsckF2fs => "fsck.f2fs",
        Program::ResizeF2fs => "resize.f2fs",
        Program::XfsGrowfs => "xfs_growfs",
        Program::XfsRepair => "xfs_repair",
        Program::XfsAdmin => "xfs_admin",
        Program::Btrfs => "btrfs",
        Program::Btrfstune => "btrfstune",
        Program::Fatresize => "fatresize",
        Program::Fatlabel => "fatlabel",
        Program::FsckVfat => "fsck.fat",
        Program::Exfatlabel => "exfatlabel",
        Program::FsckExfat => "fsck.exfat",
        Program::Mkswap => "mkswap",
        Program::Swaplabel => "swaplabel",
        Program::Pvs => "pvs",
        Program::Lvs => "lvs",
        Program::Vgs => "vgs",
        Program::Pvresize => "pvresize",
        Program::MkfsVfat => "mkfs.vfat",
        Program::MkfsExfat => "mkfs.exfat",
        Program::MkfsNtfs => "mkntfs",
        Program::MkfsXfs => "mkfs.xfs",
        Program::MkfsBtrfs => "mkfs.btrfs",
        Program::MkfsF2fs => "mkfs.f2fs",
        Program::Mount => "mount",
        Program::Umount => "umount",
        Program::Losetup => "losetup",
        Program::Sfdisk => "sfdisk",
        Program::Partx => "partx",
        Program::Blkid => "blkid",
    }
}

/// sysfs 根目录。生产取 `/sys`，测试可经环境变量指向构造的目录树。
pub fn sysfs_root() -> PathBuf {
    root_from_env("DISKEDIT_SYSFS_ROOT", "/sys")
}

/// procfs 根目录。
pub fn proc_root() -> PathBuf {
    root_from_env("DISKEDIT_PROC_ROOT", "/proc")
}

fn root_from_env(name: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        Some(_) => PathBuf::from(fallback),
        None => PathBuf::from(fallback),
    }
}

/// 读取虚拟文件系统的文本属性。
///
/// 文件不存在表示"该属性没有提供"；存在但为空返回空串；其余错误如实上抛。
pub fn read_optional_attr(path: &Path) -> PlatformResult<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PlatformError::io(path, error)),
    }
}

/// 读取必须存在的属性；不存在按解析失败报告。
pub fn read_required_attr(path: &Path) -> PlatformResult<String> {
    read_optional_attr(path)?
        .ok_or_else(|| PlatformError::parse(format!("{} 不存在", path.display())))
}
