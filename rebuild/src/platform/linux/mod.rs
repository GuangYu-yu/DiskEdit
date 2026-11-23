//! Linux 端口实现。
//!
//! 一切与内核接口、虚拟文件系统、外部程序路径有关的知识都收在本子树；
//! 上层只看见 [`crate::platform`] 里的中立类型与端口 trait。

pub mod dm;
pub mod kernel;
pub mod loopdev;
pub mod lvm;
pub mod mount;
pub mod occupancy;
pub mod records;
pub mod source;
pub mod target;

use std::path::{Path, PathBuf};
use std::process::Command;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;

use crate::platform::{
    ExitKind, Occupancy, Output, Partition, Platform, PlatformError, PlatformResult, TargetHandle,
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
        let output = self.run(Program::Partx, &["-u", &path.to_string_lossy()])?;
        if output.succeeded() {
            Ok(())
        } else {
            Err(PlatformError::tools(
                "partx",
                output.stderr_text().trim().to_string(),
            ))
        }
    }

    fn btrfs_fsid(&self, path: &Path) -> PlatformResult<[u8; 16]> {
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
        kernel::parse_fsid(&output.stdout_text())
            .ok_or_else(|| PlatformError::parse("btrfs filesystem show 输出中没有可解析的 fsid"))
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
