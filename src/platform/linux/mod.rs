//! Linux 端口实现：文件与块设备句柄、身份、占用、外部程序与临时块设备视图。
//!
//! 只有本模块出现系统调用、虚拟文件系统路径与具体可执行名；上层看到的是
//! [`crate::platform::Platform`] 与 [`crate::platform::TargetHandle`] 的中立形状。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;
use sha2::{Digest, Sha256};

use crate::platform::{
    Carrier, DeviceMatch, LoopBinding, MountFacts, Occupancy, PartitionMount, Platform,
    PlatformError, PlatformResult, StateLock, TargetHandle, TargetKind, ToolRun,
};

mod tools;

pub mod carrier;

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

    fn filesystem_ready(&self, path: &Path) -> bool {
        match self.path_kind(path) {
            TargetKind::File => true,
            TargetKind::BlockDevice => block_view_writable(path),
            TargetKind::Other => false,
        }
    }

    fn mount_facts(&self, path: &Path) -> PlatformResult<MountFacts> {
        mount_facts(path)
    }

    fn partition_mount(&self, disk: &Path, partition: u32) -> PlatformResult<PartitionMount> {
        partition_mount(disk, partition)
    }

    fn swap_active(&self, disk: &Path, partition: u32) -> PlatformResult<bool> {
        swap_active(disk, partition)
    }

    fn resize_partition_live(
        &self,
        disk: &Path,
        partition: u32,
        start_bytes: u64,
        len_bytes: u64,
    ) -> PlatformResult<()> {
        resize_partition_live(disk, partition, start_bytes, len_bytes)
    }

    fn carrier(&self) -> PlatformResult<Option<Box<dyn Carrier>>> {
        carrier::from_environment()
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

    fn occupancy_for_lvm(&self, path: &Path) -> PlatformResult<Occupancy> {
        let metadata = std::fs::metadata(path).map_err(|error| PlatformError::io(path, error))?;
        if metadata.file_type().is_block_device() {
            return block_occupancy(path, &metadata);
        }
        image_occupancy_allow_dm(path)
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
        let output = Command::new(executable)
            .args(args)
            .output()
            .map_err(|error| PlatformError::io(Path::new(executable), error))?;
        Ok(ToolRun {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn check_arguments(&self, program: Program) -> &'static [&'static str] {
        tools::check_arguments(program)
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

    fn compare_devices(&self, target: &Path, reported: &Path) -> DeviceMatch {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if target == reported {
            return DeviceMatch::Same;
        }
        let left = match std::fs::metadata(target) {
            Ok(metadata) => metadata,
            Err(error) => {
                return DeviceMatch::Unknown(format!("cannot stat {}: {error}", target.display()));
            }
        };
        let right = match std::fs::metadata(reported) {
            Ok(metadata) => metadata,
            Err(error) => {
                return DeviceMatch::Unknown(format!(
                    "cannot stat {}: {error}",
                    reported.display()
                ));
            }
        };
        let same = if left.file_type().is_block_device() && right.file_type().is_block_device() {
            left.rdev() == right.rdev()
        } else {
            left.dev() == right.dev() && left.ino() == right.ino()
        };
        if same {
            DeviceMatch::Same
        } else {
            DeviceMatch::Different
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
        use std::os::unix::fs::DirBuilderExt;
        // 记录目录链 `<state_root>/images/<键>`：逐级按 0700 建出，只有属主能读——记录里
        // 存着写前的分区表。`state_root` 之上的中间层只建不核，与基线一致；`create_dir_all`
        // 会让 `state_root`/`images` 带上 umask 的 0755，故逐级 `DirBuilder::mode(0700)`。
        let images = path
            .parent()
            .ok_or_else(|| PlatformError::parse("the record directory has no parent"))?;
        let root = images
            .parent()
            .ok_or_else(|| PlatformError::parse("the record directory has no grandparent"))?;
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(|error| PlatformError::io(parent, error))?;
        }
        for directory in [root, images, path] {
            match std::fs::DirBuilder::new().mode(0o700).create(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(PlatformError::io(directory, error)),
            }
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| PlatformError::io(directory, error))?;
        }
        Ok(())
    }

    fn remove_directory(&self, path: &Path) -> PlatformResult<()> {
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PlatformError::io(path, error)),
        }
    }

    fn temporary_directory(&self, tag: &str) -> PlatformResult<PathBuf> {
        let suffix = self.entropy(8)?;
        let name = format!("diskedit.{tag}.{}", hex(&suffix));
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).map_err(|error| PlatformError::io(&path, error))?;
        Ok(path)
    }

    fn write_state_file(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        // B5：临时名 `.正式名.<pid>.<随机>.tmp`，`O_CREAT|O_EXCL`，文件 0600；
        // 写入并落盘后原子改名到位，任何半成品都不会以正式名出现。
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PlatformError::parse("state file path has no name"))?;
        let suffix = self.entropy(8)?;
        let temporary = path.with_file_name(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            hex(&suffix)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| PlatformError::io(&temporary, error))?;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(std::fs::remove_file(&temporary));
            return Err(PlatformError::io(&temporary, error));
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temporary, path) {
            drop(std::fs::remove_file(&temporary));
            return Err(PlatformError::io(path, error));
        }
        if let Some(parent) = path.parent() {
            self.sync_directory(parent)?;
        }
        Ok(())
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

    fn refresh_partitions(&mut self) -> PlatformResult<()> {
        if !self.kind.is_block_device() {
            return Ok(());
        }
        // BLKRRPART：让内核丢弃并重读该设备的分区表。
        const BLKRRPART: libc::Ioctl = 0x125f;
        // SAFETY: fd 由打开的设备句柄保证有效，ioctl 不读取用户缓冲。
        let result = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKRRPART) };
        if result == 0 {
            Ok(())
        } else {
            Err(PlatformError::io(&self.path, io::Error::last_os_error()))
        }
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

/// 本进程能否对这块设备执行分区表重读。
///
/// 把一个路径反查成挂载事实：挂载点必须唯一，挂载源必须是分区，且能映射到单块整盘。
fn mount_facts(path: &Path) -> PlatformResult<MountFacts> {
    let mounts = std::fs::read_to_string("/proc/self/mounts")
        .map_err(|error| PlatformError::io(Path::new("/proc/self/mounts"), error))?;
    let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut sources: Vec<String> = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let target = fields.next()?;
            (Path::new(&unescape_mount(target)) == wanted.as_path()).then(|| unescape_mount(source))
        })
        .collect();
    match sources.len() {
        0 => return Ok(MountFacts::NotMounted),
        1 => {}
        _ => return Ok(MountFacts::Ambiguous),
    }
    let source = PathBuf::from(sources.remove(0));
    let device = std::fs::canonicalize(&source).unwrap_or(source);
    let metadata = match std::fs::metadata(&device) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Ok(MountFacts::Unmapped {
                why: PlatformError::io(&device, error).to_string(),
            });
        }
    };
    if !metadata.file_type().is_block_device() {
        return Ok(MountFacts::NotPartition { device });
    }
    let Some(name) = device.file_name().and_then(|name| name.to_str()) else {
        return Ok(MountFacts::Unmapped {
            why: "the mounted block device has no name".to_owned(),
        });
    };
    let node = Path::new("/sys/class/block").join(name);
    let number = match std::fs::read_to_string(node.join("partition")) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(number) => number,
            Err(_) => {
                return Ok(MountFacts::Unmapped {
                    why: format!("{name} declares an unreadable partition number"),
                });
            }
        },
        Err(_) => return Ok(MountFacts::NotPartition { device }),
    };
    let Some(disk_name) = std::fs::read_link(&node)
        .ok()
        .and_then(|target| {
            target
                .parent()
                .and_then(|parent| parent.file_name())
                .map(|n| n.to_owned())
        })
        .and_then(|name| name.to_str().map(str::to_owned))
    else {
        return Ok(MountFacts::Unmapped {
            why: format!("cannot resolve the whole disk that {name} belongs to"),
        });
    };
    Ok(MountFacts::Mounted {
        mount_point: wanted,
        device,
        disk: Path::new("/dev").join(disk_name),
        partition: number,
    })
}

/// `/proc/self/mounts` 把空格、制表、换行与反斜杠写成八进制转义。
fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte))
        {
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 目标分区的设备号（major, minor）；设备节点不存在时按"不适用"返回。
fn partition_dev_no(disk: &Path, partition: u32) -> PlatformResult<(u64, u64)> {
    let node = LinuxPlatform.partition_device(disk, partition)?;
    let metadata = std::fs::metadata(&node).map_err(|error| PlatformError::io(&node, error))?;
    let rdev = metadata.rdev();
    Ok((u64::from(libc::major(rdev)), u64::from(libc::minor(rdev))))
}

/// 一条挂载记录：设备号与挂载点。
type MountEntry = ((u64, u64), PathBuf);

/// 一个挂载命名空间的记录集。
type NamespaceMounts = (Option<u32>, Vec<MountEntry>);

/// 一个挂载命名空间里的挂载记录：设备号与挂载点。
fn read_mountinfo(path: &Path) -> io::Result<Vec<MountEntry>> {
    let text = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(_id) = fields.next() else { continue };
        let Some(_parent) = fields.next() else {
            continue;
        };
        let Some(dev) = fields.next() else { continue };
        let Some((major, minor)) = dev.split_once(':') else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse::<u64>(), minor.parse::<u64>()) else {
            continue;
        };
        let Some(_root) = fields.next() else { continue };
        let Some(mount_point) = fields.next() else {
            continue;
        };
        entries.push(((major, minor), PathBuf::from(unescape_mount(mount_point))));
    }
    Ok(entries)
}

/// 本命名空间的 `/proc/self/mountinfo`。
fn own_mountinfo() -> PlatformResult<Vec<MountEntry>> {
    read_mountinfo(Path::new("/proc/self/mountinfo"))
        .map_err(|error| PlatformError::io(Path::new("/proc/self/mountinfo"), error))
}

/// 其他可读挂载命名空间的 `mountinfo`：读不到的进程直接跳过。
fn other_mountinfo() -> Vec<NamespaceMounts> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let path = Path::new("/proc").join(&name).join("mountinfo");
        if let Ok(entries) = read_mountinfo(&path) {
            found.push((Some(pid), entries));
        }
    }
    found
}

/// 分区在全部可读挂载命名空间里的使用情况。
fn partition_mount(disk: &Path, partition: u32) -> PlatformResult<PartitionMount> {
    let target = partition_dev_no(disk, partition)?;
    let own: Vec<PathBuf> = own_mountinfo()?
        .into_iter()
        .filter(|(dev, _)| *dev == target)
        .map(|(_, mount_point)| mount_point)
        .collect();
    match own.len() {
        1 => {
            return Ok(PartitionMount::Here {
                mount_point: own.into_iter().next().expect("长度为一"),
            });
        }
        n if n > 1 => return Ok(PartitionMount::Ambiguous),
        _ => {}
    }
    for (pid, entries) in other_mountinfo() {
        if let Some((_, mount_point)) = entries.into_iter().find(|(dev, _)| *dev == target) {
            return Ok(PartitionMount::Elsewhere { mount_point, pid });
        }
    }
    Ok(PartitionMount::NotMounted)
}

/// 分区是否是活动 swap 区域；读不出的条目按环境故障上抛，不折叠成"没有"。
fn swap_active(disk: &Path, partition: u32) -> PlatformResult<bool> {
    let target = partition_dev_no(disk, partition)?;
    let text = std::fs::read_to_string("/proc/swaps")
        .map_err(|error| PlatformError::io(Path::new("/proc/swaps"), error))?;
    for line in text.lines().skip(1) {
        let Some(source) = line.split_whitespace().next() else {
            continue;
        };
        let metadata = std::fs::metadata(source).map_err(|error| {
            PlatformError::io(
                Path::new(source),
                io::Error::new(
                    error.kind(),
                    format!("cannot resolve the /proc/swaps entry {source}: {error}"),
                ),
            )
        })?;
        let rdev = metadata.rdev();
        if (u64::from(libc::major(rdev)), u64::from(libc::minor(rdev))) == target {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `blkpg_ioctl_arg` 里的 `blkpg_partition`。
#[repr(C)]
struct BlkpgPartition {
    start: i64,
    length: i64,
    pno: i32,
    devname: [libc::c_char; 64],
    volname: [libc::c_char; 64],
}

/// `BLKPG` 的 ioctl 参数头。
#[repr(C)]
struct BlkpgIoctlArg {
    op: i32,
    flags: i32,
    datalen: i32,
    data: *mut libc::c_void,
}

/// 让内核按新起点与长度调整一个已存在的分区。
fn resize_partition_live(
    disk: &Path,
    partition: u32,
    start_bytes: u64,
    len_bytes: u64,
) -> PlatformResult<()> {
    const BLKPG: libc::Ioctl = 0x1269;
    const BLKPG_RESIZE_PARTITION: i32 = 3;
    let start = i64::try_from(start_bytes).map_err(|_| {
        PlatformError::not_applicable("the partition start exceeds the ioctl range")
    })?;
    let length = i64::try_from(len_bytes).map_err(|_| {
        PlatformError::not_applicable("the partition length exceeds the ioctl range")
    })?;
    let pno = i32::try_from(partition).map_err(|_| {
        PlatformError::not_applicable("the partition number exceeds the ioctl range")
    })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)
        .map_err(|error| PlatformError::io(disk, error))?;
    let mut part = BlkpgPartition {
        start,
        length,
        pno,
        devname: [0; 64],
        volname: [0; 64],
    };
    let mut arg = BlkpgIoctlArg {
        op: BLKPG_RESIZE_PARTITION,
        flags: 0,
        datalen: std::mem::size_of::<BlkpgPartition>() as i32,
        data: &mut part as *mut BlkpgPartition as *mut libc::c_void,
    };
    // SAFETY: fd 来自刚打开的整盘设备；arg 指向的 BlkpgPartition 在调用期间存活。
    let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKPG, &mut arg) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => Err(PlatformError::not_supported(format!(
            "the kernel refuses a live partition resize without CAP_SYS_ADMIN: {error}"
        ))),
        _ => Err(PlatformError::io(disk, error)),
    }
}

/// 用真实的 `BLKRRPART` 探测，而不是 `euid`/`CapEff` 之类的代理：重读分区表是
/// 幂等操作，能执行就说明本宿主具备块层能力。`EPERM`/`EACCES` 是能力缺失；
/// `EBUSY` 等其它错误说明能力在、只是设备正被占用，不能据此判为不支持。
fn block_view_writable(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    // BLKRRPART：让内核丢弃并重读分区表。
    const BLKRRPART: libc::Ioctl = 0x125f;
    // SAFETY: fd 来自刚打开的设备；BLKRRPART 不读取用户缓冲。
    if unsafe { libc::ioctl(file.as_raw_fd(), BLKRRPART) } == 0 {
        return true;
    }
    !matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM) | Some(libc::EACCES)
    )
}

fn block_identity(path: &Path, metadata: &std::fs::Metadata) -> PlatformResult<Fingerprint> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PlatformError::parse("block device path has no name"))?;
    let Some(topology) = block_topology(name) else {
        return Ok(Fingerprint::unusable([0u8; 32]));
    };
    // 分区身份必须带上所在整盘与分区自身的位置：同一块盘的不同分区是不同的目标。
    if let Some(fingerprint) = hardware_identity(&topology, metadata) {
        return Ok(fingerprint);
    }
    // loop 设备没有 WWN/EUI/序列号，但它的承载文件是稳定事实：文件身份 + 映射
    // 偏移/长度 + 分区起点与长度足以跨次识别同一目标。
    if let Some(fingerprint) = loop_identity(&topology, metadata) {
        return Ok(fingerprint);
    }
    Ok(Fingerprint::unusable([0u8; 32]))
}

/// 块设备在 sysfs 里的拓扑：整盘节点名，以及分区起点与长度（整盘为 `None`）。
struct Topology {
    disk: String,
    partition: Option<(u64, u64)>,
}

fn block_topology(name: &str) -> Option<Topology> {
    let directory = std::fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?;
    let is_partition = directory.join("partition").exists();
    let disk = if is_partition {
        directory.parent()?.file_name()?.to_str()?.to_owned()
    } else {
        directory.file_name()?.to_str()?.to_owned()
    };
    let partition = if is_partition {
        Some((
            sysfs_u64(&directory.join("start"))?,
            sysfs_u64(&directory.join("size"))?,
        ))
    } else {
        None
    };
    Some(Topology { disk, partition })
}

fn sysfs_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn sysfs_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// 设备自身给出的稳定身份：WWN、序列号、dm/uuid、md/uuid。
fn hardware_identity(topology: &Topology, metadata: &std::fs::Metadata) -> Option<Fingerprint> {
    let base = Path::new("/sys/class/block").join(&topology.disk);
    let mut hasher = Sha256::new();
    if let Some(wwid) = sysfs_text(&base.join("device/wwid")) {
        hasher.update(b"wwid\0");
        hasher.update(wwid.as_bytes());
    } else if let Some(serial) = stable_serial(&base.join("device/serial")) {
        hasher.update(b"serial\0");
        hasher.update(serial.as_bytes());
    } else if let Some(uuid) = sysfs_text(&base.join("dm/uuid")) {
        hasher.update(b"dm\0");
        hasher.update(uuid.as_bytes());
    } else {
        let uuid = sysfs_text(&base.join("md/uuid"))?;
        hasher.update(b"md\0");
        hasher.update(uuid.as_bytes());
    }
    hasher.update(
        topology
            .partition
            .map_or(0, |(start, _)| start)
            .to_le_bytes(),
    );
    hasher.update(topology.partition.map_or(0, |(_, size)| size).to_le_bytes());
    hasher.update(metadata.len().to_le_bytes());
    Some(Fingerprint::strong(hasher.finalize().into()))
}

/// 序列号必须两次读取一致才算可重复；读一次就采信会把不稳定的值当身份。
fn stable_serial(path: &Path) -> Option<String> {
    let first = sysfs_text(path)?;
    let second = sysfs_text(path)?;
    (first == second).then_some(first)
}

/// loop 设备：承载文件身份 + 映射偏移/限长 + 分区位置。
fn loop_identity(topology: &Topology, metadata: &std::fs::Metadata) -> Option<Fingerprint> {
    let base = Path::new("/sys/class/block").join(&topology.disk);
    let backing = sysfs_text(&base.join("loop/backing_file"))?;
    let offset = sysfs_u64(&base.join("loop/offset")).unwrap_or(0);
    let sizelimit = sysfs_u64(&base.join("loop/sizelimit")).unwrap_or(0);
    // 承载文件的身份取自文件本身（设备号 + inode + 长度），不是 sysfs 路径字符串：
    // 同一路径被换成另一个文件时必须识别为不同目标。
    let file = std::fs::metadata(Path::new("/").join(&backing)).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(b"loop\0");
    hasher.update(file.dev().to_le_bytes());
    hasher.update(file.ino().to_le_bytes());
    hasher.update(file.len().to_le_bytes());
    hasher.update(offset.to_le_bytes());
    hasher.update(sizelimit.to_le_bytes());
    hasher.update(metadata.len().to_le_bytes());
    let (start, size) = topology.partition.unwrap_or((0, 0));
    hasher.update(start.to_le_bytes());
    hasher.update(size.to_le_bytes());
    Some(Fingerprint::strong(hasher.finalize().into()))
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

/// 与 [`image_occupancy`] 相同，但只为 DM 栈持有的 loop 不算占用。
///
/// 这是 LVM2 PV 收尾的口径：pvresize 只改 PV 元数据，持有该分区的 dm 栈不解析
/// 这部分字节。其余持有者（未挂载的 md、另一个 loop 等）与挂载仍按占用拒绝。
fn image_occupancy_allow_dm(path: &Path) -> PlatformResult<Occupancy> {
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
        if resolved != canonical {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let mut holders = Vec::new();
        if let Ok(entries) = std::fs::read_dir(entry.path().join("holders")) {
            for holder in entries.flatten() {
                holders.push(holder.file_name().to_string_lossy().into_owned());
            }
        }
        holders.sort();
        if !holders.is_empty() && holders.iter().all(|holder| holder.starts_with("dm-")) {
            continue;
        }
        return Ok(Occupancy::Used {
            evidence: if holders.is_empty() {
                format!("bound to a block device through {name}")
            } else {
                format!("bound through {name} and held by {}", holders.join(", "))
            },
        });
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
    match holders(Path::new(SYSFS), name) {
        Ok(names) if !names.is_empty() => Ok(Occupancy::Used {
            evidence: format!("held by {}", names.join(", ")),
        }),
        Ok(_) => {
            // 未挂载、无 holders 的多设备 btrfs 成员仍是别人的数据：superblock 记录着
            // 全网设备数，成员数大于 1 时不能当空闲分区盘处理。
            if let Some(evidence) = btrfs_multidevice(path) {
                return Ok(Occupancy::Used { evidence });
            }
            Ok(Occupancy::Idle)
        }
        // 没有 sysfs 节点：无从判定。
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Occupancy::Unknown {
            why: format!("cannot locate the holders of {name}: {error}"),
        }),
        // 来源存在却读不出：fail-closed，按已占用处理，不把"看不见"当成"没有"。
        Err(error) => Ok(Occupancy::Used {
            evidence: format!("cannot confirm the holders of {name}: {error}"),
        }),
    }
}

/// sysfs 挂载点；单元测试把它换成临时目录。
const SYSFS: &str = "/sys";
/// holders 链的最大深度：dm/md/多路径叠加不会更深。
const HOLDERS_DEPTH: usize = 8;
/// btrfs superblock 的魔数。
const BTRFS_MAGIC: &[u8; 8] = b"_BHRfS_M";
/// btrfs superblock 的三份副本偏移（64K、64M、256G）。
const BTRFS_SUPERBLOCK_OFFSETS: [u64; 3] = [0x1_0000, 0x400_0000, 0x40_0000_0000];
/// superblock 中 `num_devices` 的位置。
const BTRFS_NUM_DEVICES: std::ops::Range<usize> = 0x88..0x90;

/// 递归收集设备的上层 holders（阵列、dm、多路径）。`depth` 限制层数。
fn holders(sysfs: &Path, name: &str) -> std::io::Result<Vec<String>> {
    let base = sysfs.join("class/block").join(name);
    if !base.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} has no sysfs node", base.display()),
        ));
    }
    let mut found = Vec::new();
    let mut frontier = vec![(name.to_owned(), 0usize)];
    while let Some((current, level)) = frontier.pop() {
        if level >= HOLDERS_DEPTH {
            continue;
        }
        let directory = sysfs.join("class/block").join(&current).join("holders");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let holder = entry?.file_name().to_string_lossy().into_owned();
            if !found.contains(&holder) {
                found.push(holder.clone());
                frontier.push((holder, level + 1));
            }
        }
    }
    Ok(found)
}

/// 目标是多设备 btrfs 的成员时给出证据。
fn btrfs_multidevice(path: &Path) -> Option<String> {
    let superblock = btrfs_superblock(path)?;
    let devices = btrfs_num_devices(&superblock)?;
    (devices > 1).then(|| format!("member of a {devices}-device btrfs filesystem"))
}

/// 读第一份可用的 btrfs superblock 副本。
fn btrfs_superblock(path: &Path) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut buffer = vec![0u8; 4096];
    for offset in BTRFS_SUPERBLOCK_OFFSETS {
        let read = FileExt::read_at(&file, &mut buffer, offset).ok()?;
        if read == buffer.len() && &buffer[64..72] == BTRFS_MAGIC {
            return Some(buffer);
        }
    }
    None
}

/// 从 superblock 读 `num_devices`；不是 btrfs 或长度不足时为 `None`。
fn btrfs_num_devices(superblock: &[u8]) -> Option<u64> {
    if superblock.len() < BTRFS_NUM_DEVICES.end || &superblock[64..72] != BTRFS_MAGIC {
        return None;
    }
    Some(u64::from_le_bytes(
        superblock[BTRFS_NUM_DEVICES].try_into().ok()?,
    ))
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

/// 小写十六进制，用于给临时目录取不冲突的后缀。
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: B5
    #[test]
    fn state_files_are_private_and_written_atomically() {
        let platform = LinuxPlatform;
        let base = std::env::temp_dir().join(format!("diskedit-b5-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&base));
        std::fs::create_dir_all(&base).expect("临时根可建立");
        let directory = base.join("images").join("abcdef");
        platform
            .create_directory_all(&directory)
            .expect("记录目录可建立");
        let mode_of = |path: &Path| {
            std::fs::metadata(path)
                .expect("目录元数据")
                .permissions()
                .mode()
                & 0o777
        };
        // 整条链都只对属主开放：状态根、`images`、键目录。`create_dir_all` 会把前两级
        // 建成 umask 的 0755，故这里逐级核对，缺一级就是别的账户能遍历记录。
        assert_eq!(mode_of(&base), 0o700, "状态根只对属主开放");
        assert_eq!(
            mode_of(&base.join("images")),
            0o700,
            "images 目录只对属主开放"
        );
        assert_eq!(mode_of(&directory), 0o700, "记录目录只对属主开放");

        let file = directory.join("record");
        platform
            .write_state_file(&file, b"record")
            .expect("写状态文件");
        let file_mode = std::fs::metadata(&file)
            .expect("文件元数据")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "记录文件只对属主可读写");

        let leftovers: Vec<String> = std::fs::read_dir(&directory)
            .expect("列目录")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件: {leftovers:?}");
        std::fs::remove_dir_all(&base).expect("清理临时根");
    }

    fn temporary_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("diskedit-{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(&root).expect("临时根可建立");
        root
    }

    fn link_holder(sysfs: &Path, device: &str, holder: &str) {
        std::fs::create_dir_all(
            sysfs
                .join("class/block")
                .join(device)
                .join("holders")
                .join(holder),
        )
        .expect("holder 节点可建立");
    }

    // covers: B4
    #[test]
    fn holders_are_followed_through_every_layer() {
        let sysfs = temporary_root("b4");
        // sda1 被 md0 占用，md0 又叠在 dm-0 之上：只查一层会漏掉 dm-0。
        link_holder(&sysfs, "sda1", "md0");
        link_holder(&sysfs, "md0", "dm-0");
        let found = holders(&sysfs, "sda1").expect("可读 holders");
        assert!(found.contains(&"md0".to_owned()), "{found:?}");
        assert!(found.contains(&"dm-0".to_owned()), "{found:?}");

        // 空闲设备没有上层。
        std::fs::create_dir_all(sysfs.join("class/block/sdb")).expect("设备节点");
        assert!(holders(&sysfs, "sdb").expect("可读 holders").is_empty());

        // 超过上限的更深一层不再展开，但已有各层都保留。
        for depth in 0..HOLDERS_DEPTH + 2 {
            link_holder(&sysfs, &format!("h{depth}"), &format!("h{}", depth + 1));
        }
        link_holder(&sysfs, "sdc", "h0");
        assert!(holders(&sysfs, "sdc").expect("可读 holders").len() == HOLDERS_DEPTH);

        std::fs::remove_dir_all(&sysfs).expect("清理临时根");
    }

    // covers: B17
    #[test]
    fn a_multi_device_btrfs_member_is_occupied() {
        let mut superblock = vec![0u8; 4096];
        superblock[64..72].copy_from_slice(BTRFS_MAGIC);
        superblock[BTRFS_NUM_DEVICES].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(btrfs_num_devices(&superblock), Some(2));

        let root = temporary_root("b17");
        let image = root.join("member.img");
        let mut bytes = vec![0u8; 0x1_0000 + 4096];
        bytes[0x1_0000..0x1_0000 + 4096].copy_from_slice(&superblock);
        std::fs::write(&image, &bytes).expect("镜像可写");
        assert!(
            btrfs_multidevice(&image).is_some(),
            "多设备成员即便未挂载也应判为已占用"
        );

        superblock[BTRFS_NUM_DEVICES].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(btrfs_num_devices(&superblock), Some(1));
        bytes[0x1_0000..0x1_0000 + 4096].copy_from_slice(&superblock);
        std::fs::write(&image, &bytes).expect("镜像可写");
        assert!(btrfs_multidevice(&image).is_none(), "单设备不算占用");

        std::fs::remove_dir_all(&root).expect("清理临时根");
    }
}
