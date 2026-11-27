//! 记录位置上的文件操作：读、写、删、改名全部以父目录 fd 锚定。
//!
//! 记录目录按目标身份派生，里面的文件可能是写前分区表的原像；路径的任何一级
//! 都不跟随符号链接。父目录逐级 `O_NOFOLLOW|O_DIRECTORY` 打开后，最后一级用
//! `openat`/`unlinkat`/`renameat` 相对目录 fd 操作，因此父目录在检查之后被换成
//! 链接也不会把动作引到别处。
//!
//! 写入用同目录的临时名（`O_CREAT|O_EXCL`）创建、落盘后 `renameat` 到位；删除
//! 与改名之后都对目录 fd `fsync`，半成品不会以正式名出现。

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::platform::{PlatformError, PlatformResult, RecordEntry};

/// 读取记录位置；符号链接、目录、设备、FIFO 按"不是恢复记录"回答。
pub fn read(path: &Path) -> PlatformResult<RecordEntry> {
    entry(path).map_err(|error| PlatformError::io(path, error))
}

/// 以覆盖方式写记录位置；同目录临时名落盘后原子改名。
pub fn write(path: &Path, bytes: &[u8]) -> PlatformResult<()> {
    write_at(path, bytes).map_err(|error| PlatformError::io(path, error))
}

/// 删除记录位置；不存在视为成功。
pub fn remove(path: &Path) -> PlatformResult<()> {
    remove_at(path).map_err(|error| PlatformError::io(path, error))
}

/// 在同一目录内改名；目标已存在时原子替换。
pub fn rename(from: &Path, to: &Path) -> PlatformResult<()> {
    rename_at(from, to).map_err(|error| PlatformError::io(from, error))
}

/// 递归列出记录目录里指定文件名的条目，含符号链接与目录本身。
///
/// 不进入符号链接指向的目录：链接不是记录，也不会把链接目标的内容读进来。
pub fn list(root: &Path, name: &str) -> PlatformResult<Vec<PathBuf>> {
    let mut found = Vec::new();
    walk(root, name, &mut found).map_err(|error| PlatformError::io(root, error))?;
    found.sort();
    Ok(found)
}

fn walk(directory: &Path, name: &str, found: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if path.file_name().is_some_and(|entry| entry == name) {
            found.push(path.clone());
        }
        // `DirEntry::file_type` 不跟随符号链接：链接目录不会被递归进入。
        if file_type.is_dir() {
            walk(&path, name, found)?;
        }
    }
    Ok(())
}

/// 打开记录位置并分类；`Err` 只表示"读不到"，`NotARecoveryFile` 是明确结果。
fn entry(path: &Path) -> io::Result<RecordEntry> {
    let Some(name) = path.file_name() else {
        return Ok(RecordEntry::NotARecoveryFile);
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let directory = match open_directory(parent) {
        Ok(directory) => directory,
        Err(error) => return classify(error, Entry::Directory),
    };
    let name = c_name(name)?;
    // `O_NONBLOCK` 让 FIFO 的打开不阻塞；读到内容前先 `fstat` 判定类型。
    let raw = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if raw < 0 {
        return classify(io::Error::last_os_error(), Entry::Last);
    }
    let mut file = unsafe { File::from_raw_fd(raw) };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Ok(RecordEntry::NotARecoveryFile);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(RecordEntry::Contents(bytes))
}

fn write_at(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (directory, name) = open_parent(path)?;
    let (temporary, mut file) = create_temporary(&directory)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| rename_in(&directory, &temporary, &name));
    if result.is_err() {
        drop(unlink_in(&directory, &temporary));
    }
    result.and_then(|()| sync(&directory))
}

fn remove_at(path: &Path) -> io::Result<()> {
    let (directory, name) = open_parent(path)?;
    match unlink_in(&directory, &name) {
        Ok(()) => sync(&directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_at(from: &Path, to: &Path) -> io::Result<()> {
    let (from_directory, from_name) = open_parent(from)?;
    let (to_directory, to_name) = open_parent(to)?;
    // `renameat` 的两个叶子名都不跟随链接；父目录 fd 已在打开时锚定。
    let raw = unsafe {
        libc::renameat(
            from_directory.as_raw_fd(),
            from_name.as_ptr(),
            to_directory.as_raw_fd(),
            to_name.as_ptr(),
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // 同一个目录时只需同步一次。
    if from_directory.as_raw_fd() == to_directory.as_raw_fd() {
        return sync(&from_directory);
    }
    sync(&from_directory)?;
    sync(&to_directory)
}

/// 打开父目录并取出叶子名；父目录的每一级都不跟随符号链接。
fn open_parent(path: &Path) -> io::Result<(OwnedFd, CString)> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a record path has no final name",
        ));
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let directory = open_directory(parent)?;
    Ok((directory, c_name(name)?))
}

/// 在同目录建立一个 0600 的临时文件，名字不与已有文件冲突。
fn create_temporary(directory: &OwnedFd) -> io::Result<(CString, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    loop {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!(".record.{}.{serial}.tmp", std::process::id());
        let name = CString::new(name).expect("临时名不含 NUL");
        let raw = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw >= 0 {
            return Ok((name, unsafe { File::from_raw_fd(raw) }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
}

fn rename_in(directory: &OwnedFd, from: &CString, to: &CString) -> io::Result<()> {
    let raw = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_in(directory: &OwnedFd, name: &CString) -> io::Result<()> {
    let raw = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sync(directory: &OwnedFd) -> io::Result<()> {
    let raw = unsafe { libc::fsync(directory.as_raw_fd()) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 逐级打开目录：锚点之后每一级都带 `O_NOFOLLOW|O_DIRECTORY`。
fn open_directory(path: &Path) -> io::Result<OwnedFd> {
    let anchor = if path.is_absolute() { "/" } else { "." };
    let raw = unsafe {
        libc::open(
            CString::new(anchor)?.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut directory = unsafe { OwnedFd::from_raw_fd(raw) };
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Prefix(prefix) => c_name(prefix.as_os_str())?,
            Component::ParentDir | Component::Normal(_) => c_name(component.as_os_str())?,
        };
        let raw = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        directory = unsafe { OwnedFd::from_raw_fd(raw) };
    }
    Ok(directory)
}

/// 出错的是哪一级：只有最后一级缺失才算"没有记录"。
#[derive(Clone, Copy)]
enum Entry {
    Directory,
    Last,
}

/// 把系统调用错误分成"不存在""不是记录""读不到"。
fn classify(error: io::Error, level: Entry) -> io::Result<RecordEntry> {
    match error.raw_os_error() {
        // 中间一级是符号链接、或者不是目录。
        Some(libc::ELOOP | libc::ENOTDIR) if matches!(level, Entry::Directory) => {
            Ok(RecordEntry::NotARecoveryFile)
        }
        Some(libc::ELOOP) => Ok(RecordEntry::NotARecoveryFile),
        // Unix socket 以 O_RDONLY|O_NONBLOCK 打开会得到 ENXIO。最后一级的任何
        // 非普通文件都归为"不是恢复文件"，不能落到"不可读"。
        Some(libc::ENXIO) if matches!(level, Entry::Last) => Ok(RecordEntry::NotARecoveryFile),
        Some(libc::ENOENT) => Ok(RecordEntry::Absent),
        _ => Err(error),
    }
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path component contains a NUL byte",
        )
    })
}
