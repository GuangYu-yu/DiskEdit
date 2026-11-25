//! Linux 端口实现：文件与块设备句柄、身份、占用、外部程序与临时块设备视图。
//!
//! 只有本模块出现系统调用、虚拟文件系统路径与具体可执行名；上层看到的是
//! [`super::Platform`] 与 [`super::TargetHandle`] 的中立形状。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;
use sha2::{Digest, Sha256};

use super::tools;
use super::{
    LoopBinding, Occupancy, Platform, PlatformError, PlatformResult, StateLock, TargetHandle,
    TargetKind, ToolRun,
};

/// Linux 宿主端口。
#[derive(Debug, Default)]
pub struct LinuxPlatform;

impl Platform for LinuxPlatform {
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        LinuxHandle::open(path.to_path_buf(), file)
    }

    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        LinuxHandle::open(path.to_path_buf(), file)
    }

    fn path_kind(&self, path: &Path) -> TargetKind {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.file_type().is_block_device() => TargetKind::BlockDevice,
            Ok(metadata) if metadata.is_file() => TargetKind::File,
            _ => TargetKind::Other,
        }
    }

    fn is_partition_node(&self, path: &Path) -> bool {
        let Ok(metadata) = std::fs::metadata(path) else {
            return false;
        };
        if !metadata.file_type().is_block_device() {
            return false;
        }
        let Ok(resolved) = std::fs::canonicalize(path) else {
            return false;
        };
        let Some(name) = resolved.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        Path::new("/sys/class/block")
            .join(name)
            .join("partition")
            .exists()
    }

    fn identity(&self, path: &Path) -> PlatformResult<Fingerprint> {
        let metadata = std::fs::metadata(path).map_err(|error| PlatformError::io(path, error))?;
        if metadata.file_type().is_block_device() {
            return block_identity(path, &metadata);
        }
        let mut hasher = Sha256::new();
        hasher.update(b"file\0");
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
        hasher.update(metadata.len().to_le_bytes());
        Ok(Fingerprint::strong(hasher.finalize().into()))
    }

    fn occupancy(&self, path: &Path) -> PlatformResult<Occupancy> {
        let metadata = std::fs::metadata(path).map_err(|error| PlatformError::io(path, error))?;
        if metadata.file_type().is_block_device() {
            block_occupancy(path, &metadata)
        } else {
            image_occupancy(path)
        }
    }

    fn state_root(&self) -> Option<PathBuf> {
        if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
            return Some(PathBuf::from(root).join("diskedit"));
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/diskedit"))
    }

    fn is_directory(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn list_files(&self, root: &Path) -> PlatformResult<Vec<PathBuf>> {
        let mut found = Vec::new();
        walk(root, &mut found)?;
        found.sort();
        Ok(found)
    }

    fn read_file(&self, path: &Path) -> PlatformResult<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(PlatformError::io(path, error)),
        }
    }

    fn run(&self, program: Program, args: &[String]) -> PlatformResult<ToolRun> {
        let executable = tools::executable(program);
        let fixed = tools::check_arguments(program);
        let output = Command::new(executable)
            .args(fixed)
            .args(args)
            .output()
            .map_err(|error| PlatformError::io(Path::new(executable), error))?;
        Ok(ToolRun {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn tool_available(&self, program: Program) -> bool {
        tools::locate(program).is_some()
    }

    fn tool_name(&self, program: Program) -> &'static str {
        tools::executable(program)
    }

    fn bind_loop(&self, path: &Path, offset: u64, len: u64) -> PlatformResult<LoopBinding> {
        let output = Command::new(tools::executable(Program::Losetup))
            .arg("--find")
            .arg("--show")
            .arg("--offset")
            .arg(offset.to_string())
            .arg("--sizelimit")
            .arg(len.to_string())
            .arg(path)
            .output()
            .map_err(|error| PlatformError::io(path, error))?;
        if !output.status.success() {
            return Err(PlatformError::not_applicable(format!(
                "cannot bind a block-device view for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let device = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if device.is_empty() {
            return Err(PlatformError::parse("losetup reported no device"));
        }
        Ok(LoopBinding::new(PathBuf::from(device)))
    }

    fn partition_device(&self, disk: &Path, number: u32) -> PlatformResult<PathBuf> {
        let name = disk
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PlatformError::parse("block device path has no name"))?;
        let candidate = if name.ends_with(|ch: char| ch.is_ascii_digit()) {
            disk.with_file_name(format!("{name}p{number}"))
        } else {
            disk.with_file_name(format!("{name}{number}"))
        };
        if candidate.exists() {
            Ok(candidate)
        } else {
            Err(PlatformError::not_applicable(format!(
                "partition {number} of {} has no device node",
                disk.display()
            )))
        }
    }

    fn entropy(&self, len: usize) -> PlatformResult<Vec<u8>> {
        let path = Path::new("/dev/urandom");
        let mut file = File::open(path).map_err(|error| PlatformError::io(path, error))?;
        let mut bytes = vec![0u8; len];
        file.read_exact(&mut bytes)
            .map_err(|error| PlatformError::io(path, error))?;
        Ok(bytes)
    }

    fn create_directory_all(&self, path: &Path) -> PlatformResult<()> {
        std::fs::create_dir_all(path).map_err(|error| PlatformError::io(path, error))
    }

    fn write_state_file(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        file.write_all(bytes)
            .map_err(|error| PlatformError::io(path, error))?;
        file.sync_all()
            .map_err(|error| PlatformError::io(path, error))
    }

    fn append_state_file(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        file.write_all(bytes)
            .map_err(|error| PlatformError::io(path, error))?;
        file.sync_all()
            .map_err(|error| PlatformError::io(path, error))
    }

    fn remove_state_file(&self, path: &Path) -> PlatformResult<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PlatformError::io(path, error)),
        }
    }

    fn sync_directory(&self, path: &Path) -> PlatformResult<()> {
        let directory = File::open(path).map_err(|error| PlatformError::io(path, error))?;
        directory
            .sync_all()
            .map_err(|error| PlatformError::io(path, error))
    }

    fn lock_state(&self, path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(PlatformError::io(path, io::Error::last_os_error()));
        }
        Ok(Box::new(ExclusiveLock { _file: file }))
    }
}

/// 文件描述符关闭即释放 `flock`；字段只为持有生命周期。
struct ExclusiveLock {
    _file: File,
}

impl StateLock for ExclusiveLock {}

impl Drop for LoopBinding {
    fn drop(&mut self) {
        drop(
            Command::new(tools::executable(Program::Losetup))
                .arg("--detach")
                .arg(&self.device)
                .output(),
        );
    }
}

/// 已打开的 Linux 目标。
#[derive(Debug)]
struct LinuxHandle {
    path: PathBuf,
    file: File,
    len: u64,
    kind: TargetKind,
    sector_size: Option<u64>,
}

impl LinuxHandle {
    fn open(path: PathBuf, file: File) -> PlatformResult<Box<dyn TargetHandle>> {
        let metadata = file
            .metadata()
            .map_err(|error| PlatformError::io(&path, error))?;
        let kind = if metadata.file_type().is_block_device() {
            TargetKind::BlockDevice
        } else if metadata.is_file() {
            TargetKind::File
        } else {
            TargetKind::Other
        };
        let len = if kind.is_block_device() {
            let end = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_END) };
            if end < 0 {
                return Err(PlatformError::io(&path, io::Error::last_os_error()));
            }
            end as u64
        } else {
            metadata.len()
        };
        let sector_size = if kind.is_block_device() {
            block_sector_size(&file)
        } else {
            None
        };
        Ok(Box::new(Self {
            path,
            file,
            len,
            kind,
            sector_size,
        }))
    }
}

impl TargetHandle for LinuxHandle {
    fn path(&self) -> &Path {
        &self.path
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn kind(&self) -> TargetKind {
        self.kind
    }

    fn sector_size(&self) -> Option<u64> {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        FileExt::read_at(&self.file, buf, offset)
            .map_err(|error| PlatformError::io(&self.path, error))
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        FileExt::write_at(&self.file, buf, offset)
            .map_err(|error| PlatformError::io(&self.path, error))
    }

    fn barrier(&mut self) -> PlatformResult<()> {
        self.file
            .sync_all()
            .map_err(|error| PlatformError::io(&self.path, error))
    }
}

fn block_sector_size(file: &File) -> Option<u64> {
    let mut value: libc::c_uint = 0;
    // SAFETY: BLKSSZGET 把逻辑扇区字节数写入 c_uint；fd 由打开的句柄保证有效。
    let result = unsafe { libc::ioctl(file.as_raw_fd(), libc::BLKSSZGET, &mut value) };
    if result == 0 && value > 0 {
        Some(u64::from(value))
    } else {
        None
    }
}

fn block_identity(path: &Path, metadata: &std::fs::Metadata) -> PlatformResult<Fingerprint> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PlatformError::parse("block device path has no name"))?;
    let base = name
        .trim_end_matches(|ch: char| ch.is_ascii_digit())
        .trim_end_matches('p');
    for candidate in [
        format!("{base}/device/wwid"),
        format!("{base}/device/serial"),
    ] {
        let sysfs = Path::new("/sys/block").join(&candidate);
        if let Ok(text) = std::fs::read_to_string(&sysfs) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let mut hasher = Sha256::new();
                hasher.update(b"block\0");
                hasher.update(trimmed.as_bytes());
                hasher.update(metadata.len().to_le_bytes());
                return Ok(Fingerprint::strong(hasher.finalize().into()));
            }
        }
    }
    Ok(Fingerprint::unusable([0u8; 32]))
}

fn image_occupancy(path: &Path) -> PlatformResult<Occupancy> {
    let canonical = std::fs::canonicalize(path).map_err(|error| PlatformError::io(path, error))?;
    let block = match std::fs::read_dir("/sys/block") {
        Ok(entries) => entries,
        Err(error) => {
            return Ok(Occupancy::Unknown {
                why: format!("cannot enumerate block devices: {error}"),
            });
        }
    };
    for entry in block.flatten() {
        let backing = entry.path().join("loop/backing_file");
        let Ok(text) = std::fs::read_to_string(&backing) else {
            continue;
        };
        let target = text.trim();
        if target.is_empty() {
            continue;
        }
        let resolved = std::fs::canonicalize(Path::new("/").join(target))
            .unwrap_or_else(|_| PathBuf::from(target));
        if resolved == canonical {
            return Ok(Occupancy::Used {
                evidence: format!(
                    "bound to a block device through {}",
                    entry.file_name().to_string_lossy()
                ),
            });
        }
    }
    Ok(Occupancy::Idle)
}

fn block_occupancy(path: &Path, _metadata: &std::fs::Metadata) -> PlatformResult<Occupancy> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts")
        && let Some(mount) = mounts
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .find(|source| Path::new(source) == canonical)
    {
        return Ok(Occupancy::Used {
            evidence: format!("mounted from {mount}"),
        });
    }
    if let Ok(swaps) = std::fs::read_to_string("/proc/swaps")
        && swaps
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            .any(|source| Path::new(source) == canonical)
    {
        return Ok(Occupancy::Used {
            evidence: "active swap area".to_owned(),
        });
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PlatformError::parse("block device path has no name"))?;
    let holders = Path::new("/sys/class/block").join(name).join("holders");
    match std::fs::read_dir(&holders) {
        Ok(entries) => {
            let names: Vec<String> = entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            if names.is_empty() {
                Ok(Occupancy::Idle)
            } else {
                Ok(Occupancy::Used {
                    evidence: format!("held by {}", names.join(", ")),
                })
            }
        }
        Err(error) => Ok(Occupancy::Unknown {
            why: format!("cannot read {}: {error}", holders.display()),
        }),
    }
}

fn walk(root: &Path, found: &mut Vec<PathBuf>) -> PlatformResult<()> {
    let entries = std::fs::read_dir(root).map_err(|error| PlatformError::io(root, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(root, error))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| PlatformError::io(&path, error))?;
        if file_type.is_dir() {
            walk(&path, found)?;
        } else if file_type.is_file() {
            found.push(path);
        }
    }
    Ok(())
}
