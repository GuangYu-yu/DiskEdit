//! Linux 端口实现：文件与块设备句柄、身份、占用、外部程序与临时块设备视图。
//!
//! 只有本模块出现系统调用、虚拟文件系统路径与具体可执行名；上层看到的是
//! [`crate::platform::Platform`] 与 [`crate::platform::TargetHandle`] 的中立形状。

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;
use sha2::{Digest, Sha256};

use crate::platform::{
    Carrier, DeviceMatch, LoopBinding, MountFacts, Occupancy, PartitionMount, Platform,
    PlatformError, PlatformResult, RecordEntry, StateLock, TargetHandle, TargetKind, ToolRun,
};

mod record;
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

    fn list_record_entries(&self, root: &Path, name: &str) -> PlatformResult<Vec<PathBuf>> {
        record::list(root, name)
    }

    fn read_record(&self, path: &Path) -> PlatformResult<RecordEntry> {
        record::read(path)
    }

    fn write_record(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        record::write(path, bytes)
    }

    fn remove_record(&self, path: &Path) -> PlatformResult<()> {
        record::remove(path)
    }

    fn rename_record(&self, from: &Path, to: &Path) -> PlatformResult<()> {
        record::rename(from, to)
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
        // 存着写前的分区表。`state_root` 之上的中间层只建不核；`create_dir_all`
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
        // 返回"此刻长度"而不是打开时的缓存：普通文件可能被外部截断或增长，
        // 块设备的分区也可能被重新划分。查询失败时才退回打开时的值。
        match self.kind {
            TargetKind::BlockDevice => {
                let end = unsafe { libc::lseek(self.file.as_raw_fd(), 0, libc::SEEK_END) };
                if end < 0 { self.len } else { end as u64 }
            }
            TargetKind::File | TargetKind::Other => {
                self.file.metadata().map_or(self.len, |meta| meta.len())
            }
        }
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
        // 普通文件的长度会随外部截断而变：每次写入前取此刻长度，不用打开时缓存，
        // 否则越界写会静默稀疏扩展目标。
        if self.kind == TargetKind::File {
            let current = self
                .file
                .metadata()
                .map_err(|error| PlatformError::io(&self.path, error))?
                .len();
            let end = offset.checked_add(buf.len() as u64).ok_or_else(|| {
                PlatformError::io(
                    &self.path,
                    io::Error::new(io::ErrorKind::InvalidInput, "write range overflows u64"),
                )
            })?;
            if end > current {
                return Err(PlatformError::io(
                    &self.path,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("write [{offset}, {end}) extends past the target length {current}"),
                    ),
                ));
            }
        }
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
    let sysfs = Path::new("/sys/class/block").join(&topology.disk);
    if let Some(fingerprint) = loop_identity(&sysfs, &topology, metadata) {
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
///
/// `sysfs` 是该设备的 sysfs 目录；映射事实缺失时返回 `None`，不按 0 猜。
fn loop_identity(
    sysfs: &Path,
    topology: &Topology,
    metadata: &std::fs::Metadata,
) -> Option<Fingerprint> {
    let backing = sysfs_text(&sysfs.join("loop/backing_file"))?;
    let offset = sysfs_u64(&sysfs.join("loop/offset"))?;
    let sizelimit = sysfs_u64(&sysfs.join("loop/sizelimit"))?;
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
    image_occupancy_in(Path::new(SYSFS), path, false)
}

/// 与 [`image_occupancy`] 相同，但只为 DM 栈持有的 loop 不算占用。
///
/// 这是 LVM2 PV 收尾的占用判定：pvresize 只改 PV 元数据，持有该分区的 dm 栈不解析
/// 这部分字节。其余持有者（未挂载的 md、另一个 loop 等）与挂载仍按占用拒绝。
fn image_occupancy_allow_dm(path: &Path) -> PlatformResult<Occupancy> {
    image_occupancy_in(Path::new(SYSFS), path, true)
}

/// 一个 loop 设备的承载文件；没有承载文件时为空。
///
/// 读不出来是环境故障，不能当作"这个 loop 没在用"：那会把占用结论折成空闲。
fn loop_backing_file(entry: &Path) -> std::io::Result<Option<PathBuf>> {
    let text = match std::fs::read_to_string(entry.join("loop/backing_file")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let target = text.trim();
    if target.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from("/").join(target)))
}

/// 镜像的占用判定：任何 loop 绑定、任何多设备 btrfs 成员都算占用。
///
/// `allow_dm` 时只为 DM 栈持有的 loop 不计入（见 [`image_occupancy_allow_dm`]）。
fn image_occupancy_in(sysfs: &Path, path: &Path, allow_dm: bool) -> PlatformResult<Occupancy> {
    let canonical = std::fs::canonicalize(path).map_err(|error| PlatformError::io(path, error))?;
    let block = match std::fs::read_dir(sysfs.join("block")) {
        Ok(entries) => entries,
        Err(error) => {
            return Ok(Occupancy::Unknown {
                why: format!("cannot enumerate block devices: {error}"),
            });
        }
    };
    for entry in block {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Ok(Occupancy::Unknown {
                    why: format!("cannot enumerate block devices: {error}"),
                });
            }
        };
        let backing = match loop_backing_file(&entry.path()) {
            Ok(Some(backing)) => backing,
            Ok(None) => continue,
            Err(error) => {
                return Ok(Occupancy::Unknown {
                    why: format!(
                        "cannot read the loop backing file of {}: {error}",
                        entry.file_name().to_string_lossy()
                    ),
                });
            }
        };
        let resolved = std::fs::canonicalize(&backing).unwrap_or(backing);
        if resolved != canonical {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let mut holders = Vec::new();
        if allow_dm && let Ok(entries) = std::fs::read_dir(entry.path().join("holders")) {
            for holder in entries.flatten() {
                holders.push(holder.file_name().to_string_lossy().into_owned());
            }
            holders.sort();
            if !holders.is_empty() && holders.iter().all(|holder| holder.starts_with("dm-")) {
                continue;
            }
        }
        return Ok(Occupancy::Used {
            evidence: if holders.is_empty() {
                format!("bound to a block device through {name}")
            } else {
                format!("bound through {name} and held by {}", holders.join(", "))
            },
        });
    }
    btrfs_occupancy(path, "the image holds no loop binding")
}

/// 多设备 btrfs 成员：未挂载、没有 loop 绑定也要判为占用。
fn btrfs_occupancy(path: &Path, idle_context: &str) -> PlatformResult<Occupancy> {
    match btrfs_membership(path) {
        BtrfsMembers::Multi(facts) => Ok(Occupancy::Used {
            evidence: format!(
                "member {} (device {}) of a {}-device btrfs filesystem {}",
                hex(&facts.member.uuid),
                facts.member.devid,
                facts.devices,
                hex(&facts.fsid)
            ),
        }),
        BtrfsMembers::Single | BtrfsMembers::NotBtrfs => Ok(Occupancy::Idle),
        BtrfsMembers::Unreadable(why) => Ok(Occupancy::Unknown {
            why: format!("{idle_context}, and its btrfs superblock is unreadable: {why}"),
        }),
    }
}

fn block_occupancy(path: &Path, _metadata: &std::fs::Metadata) -> PlatformResult<Occupancy> {
    block_occupancy_in(Path::new(SYSFS), path)
}

fn block_occupancy_in(sysfs: &Path, path: &Path) -> PlatformResult<Occupancy> {
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
    match holders(sysfs, name) {
        Ok(names) if !names.is_empty() => Ok(Occupancy::Used {
            evidence: format!("held by {}", names.join(", ")),
        }),
        Ok(_) => {
            // 未挂载、无 holders 的多设备 btrfs 成员仍是别人的数据：superblock 记录着
            // 全网设备数，成员数大于 1 时不能当空闲分区盘处理。
            btrfs_occupancy(path, "no holders")
        }
        // 没有 sysfs 节点：无从判定。
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Occupancy::Unknown {
            why: format!("cannot locate the holders of {name}: {error}"),
        }),
        // 来源存在却读不出：读失败是未知，不是"没有占用"。
        Err(error) => Ok(Occupancy::Unknown {
            why: format!("cannot read the holders of {name}: {error}"),
        }),
    }
}

/// sysfs 挂载点；单元测试把它换成临时目录。
const SYSFS: &str = "/sys";
/// holders 链的最大深度：dm/md/多路径叠加不会更深。
const HOLDERS_DEPTH: usize = 8;
/// btrfs superblock 的魔数。
const BTRFS_MAGIC: &[u8; 8] = b"_BHRfS_M";
/// superblock 中魔数的位置。
const BTRFS_MAGIC_AT: std::ops::Range<usize> = 64..72;
/// btrfs superblock 的三份副本偏移（64K、64M、256G）。
const BTRFS_SUPERBLOCK_OFFSETS: [u64; 3] = [0x1_0000, 0x400_0000, 0x40_0000_0000];
/// UUID 的字节数。
const BTRFS_UUID_SIZE: usize = 16;
/// superblock 中文件系统 UUID 的位置；所有成员共享同一个值。
const BTRFS_FSID: std::ops::Range<usize> = 0x20..0x30;
/// superblock 中 `num_devices` 的位置。
const BTRFS_NUM_DEVICES: std::ops::Range<usize> = 0x88..0x90;
/// superblock 中本设备 `DEV_ITEM` 的位置。
const BTRFS_DEV_ITEM: std::ops::Range<usize> = 0xC9..0x12B;
/// `DEV_ITEM` 中 `devid` 的位置。
const BTRFS_DEV_ITEM_DEVID: std::ops::Range<usize> = 0xC9..0xD1;
/// `DEV_ITEM` 中本设备 UUID 的位置。
const BTRFS_DEV_ITEM_UUID: std::ops::Range<usize> = 0x10B..0x11B;

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

/// superblock 中 `DEV_ITEM` 记录的设备标识：设备号与本设备 UUID。
#[derive(Debug, PartialEq, Eq)]
struct BtrfsMember {
    /// 本设备在文件系统内的设备号。
    devid: u64,
    /// btrfs 为本设备生成的 UUID。
    uuid: [u8; BTRFS_UUID_SIZE],
}

/// 一份 superblock 能给出的成员事实。
#[derive(Debug, PartialEq, Eq)]
struct BtrfsFacts {
    /// 文件系统声明的成员总数。
    devices: u64,
    /// 本设备的标识。
    member: BtrfsMember,
    /// 文件系统 UUID，所有成员共享。
    fsid: [u8; BTRFS_UUID_SIZE],
}

/// 目标在 btrfs 里的成员身份：不是 btrfs、单设备、多设备成员、或读不出来。
#[derive(Debug, PartialEq, Eq)]
enum BtrfsMembers {
    NotBtrfs,
    Single,
    /// 多设备文件系统；`member` 是本次读到的这台设备的标识。
    Multi(BtrfsFacts),
    Unreadable(String),
}

/// 读超级块判断成员身份；读失败或字段不全都是未知，不是"不是 btrfs"。
fn btrfs_membership(path: &Path) -> BtrfsMembers {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return BtrfsMembers::Unreadable(format!("cannot open {}: {error}", path.display()));
        }
    };
    let mut buffer = vec![0u8; 4096];
    let mut first_error = None;
    for (index, offset) in BTRFS_SUPERBLOCK_OFFSETS.into_iter().enumerate() {
        let read = match FileExt::read_at(&file, &mut buffer, offset) {
            Ok(read) => read,
            Err(error) => {
                // 第一份副本读不出来才是"读故障"；后面的副本在短设备上可能越界，
                // 那不影响"这是不是 btrfs"的判断。
                if index == 0 {
                    first_error = Some(format!(
                        "cannot read the btrfs superblock of {} at {offset}: {error}",
                        path.display()
                    ));
                }
                continue;
            }
        };
        if read < BTRFS_MAGIC_AT.end || &buffer[BTRFS_MAGIC_AT] != BTRFS_MAGIC {
            continue;
        }
        // 魔数已确认是 btrfs：字段读不全就不再往下猜，按未知处理。
        return match btrfs_facts(&buffer[..read]) {
            Some(facts) if facts.devices > 1 => BtrfsMembers::Multi(facts),
            Some(_) => BtrfsMembers::Single,
            None => BtrfsMembers::Unreadable(format!(
                "the btrfs superblock of {} at {offset} is too short to hold a device item",
                path.display()
            )),
        };
    }
    match first_error {
        Some(why) => BtrfsMembers::Unreadable(why),
        None => BtrfsMembers::NotBtrfs,
    }
}

/// 从 superblock 读成员总数、本设备标识与文件系统 UUID。
///
/// 不是 btrfs、字段不完整或成员数为零时为 `None`；调用方按未知处理。
fn btrfs_facts(superblock: &[u8]) -> Option<BtrfsFacts> {
    if superblock.len() < BTRFS_DEV_ITEM.end || &superblock[BTRFS_MAGIC_AT] != BTRFS_MAGIC {
        return None;
    }
    let devices = u64::from_le_bytes(superblock[BTRFS_NUM_DEVICES].try_into().ok()?);
    if devices == 0 {
        return None;
    }
    let devid = u64::from_le_bytes(superblock[BTRFS_DEV_ITEM_DEVID].try_into().ok()?);
    let uuid: [u8; BTRFS_UUID_SIZE] = superblock[BTRFS_DEV_ITEM_UUID].try_into().ok()?;
    let fsid: [u8; BTRFS_UUID_SIZE] = superblock[BTRFS_FSID].try_into().ok()?;
    Some(BtrfsFacts {
        devices,
        member: BtrfsMember { devid, uuid },
        fsid,
    })
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
        platform.write_record(&file, b"record").expect("写记录文件");
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

    #[test]
    fn a_multi_device_btrfs_member_is_occupied() {
        let uuid = [0x11u8; BTRFS_UUID_SIZE];
        let fsid = [0x22u8; BTRFS_UUID_SIZE];
        let mut superblock = vec![0u8; 4096];
        superblock[BTRFS_MAGIC_AT].copy_from_slice(BTRFS_MAGIC);
        superblock[BTRFS_FSID].copy_from_slice(&fsid);
        superblock[BTRFS_NUM_DEVICES].copy_from_slice(&2u64.to_le_bytes());
        superblock[BTRFS_DEV_ITEM_DEVID].copy_from_slice(&3u64.to_le_bytes());
        superblock[BTRFS_DEV_ITEM_UUID].copy_from_slice(&uuid);
        assert_eq!(
            btrfs_facts(&superblock),
            Some(BtrfsFacts {
                devices: 2,
                member: BtrfsMember { devid: 3, uuid },
                fsid,
            })
        );

        let root = temporary_root("b17");
        let image = root.join("member.img");
        let mut bytes = vec![0u8; 0x1_0000 + 4096];
        bytes[0x1_0000..0x1_0000 + 4096].copy_from_slice(&superblock);
        std::fs::write(&image, &bytes).expect("镜像可写");
        assert_eq!(
            btrfs_membership(&image),
            BtrfsMembers::Multi(BtrfsFacts {
                devices: 2,
                member: BtrfsMember { devid: 3, uuid },
                fsid,
            }),
            "多设备成员即便未挂载也应判为已占用"
        );
        // 镜像形态也要走同一条判定：没有 loop 绑定不代表它是空闲的。
        let empty_sysfs = root.join("empty-sysfs");
        std::fs::create_dir_all(empty_sysfs.join("block")).expect("空 sysfs 可建立");
        let Occupancy::Used { evidence } =
            image_occupancy_in(&empty_sysfs, &image, false).expect("可判定")
        else {
            panic!("多设备成员应判为已占用");
        };
        assert!(evidence.contains(&hex(&uuid)), "{evidence}");
        assert!(evidence.contains(&hex(&fsid)), "{evidence}");

        superblock[BTRFS_NUM_DEVICES].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(btrfs_facts(&superblock).map(|facts| facts.devices), Some(1));
        bytes[0x1_0000..0x1_0000 + 4096].copy_from_slice(&superblock);
        std::fs::write(&image, &bytes).expect("镜像可写");
        assert_eq!(
            btrfs_membership(&image),
            BtrfsMembers::Single,
            "单设备不算多设备成员"
        );
        assert!(matches!(
            image_occupancy_in(&empty_sysfs, &image, false).expect("可判定"),
            Occupancy::Idle
        ));

        std::fs::remove_dir_all(&root).expect("清理临时根");
    }

    #[test]
    fn an_incomplete_btrfs_superblock_is_unknown_not_single() {
        let root = temporary_root("b18");
        let image = root.join("truncated.img");
        let mut bytes = vec![0u8; 0x1_0000 + 0x100];
        bytes[0x1_0000 + BTRFS_MAGIC_AT.start..0x1_0000 + BTRFS_MAGIC_AT.end]
            .copy_from_slice(BTRFS_MAGIC);
        std::fs::write(&image, &bytes).expect("镜像可写");
        assert!(
            matches!(btrfs_membership(&image), BtrfsMembers::Unreadable(_)),
            "魔数之后字段不全必须报未知，不能折成单设备"
        );
        let sysfs = root.join("sysfs");
        std::fs::create_dir_all(sysfs.join("block")).expect("空 sysfs 可建立");
        assert!(
            matches!(
                image_occupancy_in(&sysfs, &image, false).expect("可判定"),
                Occupancy::Unknown { .. }
            ),
            "字段不全不能当成空闲"
        );

        // 成员数为零不是有效的 btrfs 超级块：同样按未知处理。
        let mut superblock = vec![0u8; 4096];
        superblock[BTRFS_MAGIC_AT].copy_from_slice(BTRFS_MAGIC);
        bytes.resize(0x1_0000 + 4096, 0);
        bytes[0x1_0000..0x1_0000 + 4096].copy_from_slice(&superblock);
        std::fs::write(&image, &bytes).expect("镜像可写");
        assert!(matches!(
            btrfs_membership(&image),
            BtrfsMembers::Unreadable(_)
        ));

        std::fs::remove_dir_all(&root).expect("清理临时根");
    }

    #[test]
    fn an_unreadable_occupancy_source_is_unknown_not_idle() {
        let root = temporary_root("r23");
        let sysfs = root.join("sysfs");
        // loop 设备目录存在，但承载文件读不出来（这里用一个目录占住该路径）。
        std::fs::create_dir_all(sysfs.join("block/loop0/loop/backing_file"))
            .expect("不可读的承载文件节点可建立");
        let image = root.join("idle.img");
        std::fs::write(&image, vec![0u8; 4096]).expect("镜像可写");
        assert!(
            matches!(
                image_occupancy_in(&sysfs, &image, false).expect("可判定"),
                Occupancy::Unknown { .. }
            ),
            "sysfs 读失败必须是未知，不能当成空闲"
        );

        // holders 来源存在却读不出来（同样用目录占位）：未知，不是空闲。
        let block_sysfs = root.join("block-sysfs");
        std::fs::create_dir_all(block_sysfs.join("class/block/sdz1")).expect("设备节点可建立");
        std::fs::write(block_sysfs.join("class/block/sdz1/holders"), b"x")
            .expect("写成普通文件使 read_dir 失败");
        assert!(
            matches!(
                block_occupancy_in(&block_sysfs, Path::new("/dev/sdz1")).expect("可判定"),
                Occupancy::Unknown { .. }
            ),
            "holders 读失败必须是未知，不能当成没有占用"
        );

        // 目标自己的超级块读不出来：同样是未知，不是"不是 btrfs"。
        let directory = root.join("not-a-file");
        std::fs::create_dir_all(&directory).expect("目录可建立");
        assert!(
            matches!(btrfs_membership(&directory), BtrfsMembers::Unreadable(_)),
            "打不开的目标必须报未知"
        );

        std::fs::remove_dir_all(&root).expect("清理临时根");
    }

    #[test]
    fn a_loop_identity_without_its_mapping_facts_is_not_guessed() {
        // sysfs 缺 `loop/sizelimit` 时，身份不能把缺失当成 0：映射偏移与限长是
        // 跨次识别同一目标的依据，猜一个值继续就会把不同目标认成同一个。
        let root = temporary_root("loop-identity");
        let sysfs = root.join("sysfs");
        std::fs::create_dir_all(sysfs.join("loop")).expect("临时 sysfs 可建立");
        let backing = root.join("backing.img");
        std::fs::write(&backing, b"x").expect("承载文件可写");
        std::fs::write(
            sysfs.join("loop/backing_file"),
            format!("{}\n", backing.display()),
        )
        .expect("backing_file 可写");
        std::fs::write(sysfs.join("loop/offset"), b"0\n").expect("offset 可写");
        let metadata = std::fs::metadata(&backing).expect("元数据可读");
        let topology = Topology {
            disk: "loop0".to_owned(),
            partition: None,
        };
        assert!(
            loop_identity(&sysfs, &topology, &metadata).is_none(),
            "缺少 sizelimit 时不得按 0 拼出身份"
        );

        std::fs::write(sysfs.join("loop/sizelimit"), b"0\n").expect("sizelimit 可写");
        assert!(
            loop_identity(&sysfs, &topology, &metadata).is_some(),
            "映射事实齐全时应给出身份"
        );

        std::fs::remove_dir_all(&root).expect("清理临时根");
    }
}
