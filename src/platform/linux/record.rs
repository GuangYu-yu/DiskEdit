//! 记录位置上的读取：逐级 `openat`，任何一级都不跟随符号链接。
//!
//! 记录位置只接受普通文件。父目录逐级用 `O_NOFOLLOW|O_DIRECTORY` 打开，最后
//! 一级用 `O_NOFOLLOW` 打开并 `fstat` 确认是普通文件；符号链接、目录、设备、
//! FIFO 都按"不是恢复记录"回答，不折成"没有记录"。

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::platform::{PlatformError, PlatformResult, RecordEntry};

/// 记录目录里代表记录的文件名。
const RECORD_FILE: &str = "record";

/// 读取记录位置。
pub fn read(path: &Path) -> PlatformResult<RecordEntry> {
    entry(path).map_err(|error| PlatformError::io(path, error))
}

/// 递归列出记录目录里名为 `record` 的条目，含符号链接与目录本身。
///
/// 不进入符号链接指向的目录：链接不是记录，也不会把链接目标的内容读进来。
pub fn list(root: &Path) -> PlatformResult<Vec<PathBuf>> {
    let mut found = Vec::new();
    walk(root, &mut found).map_err(|error| PlatformError::io(root, error))?;
    found.sort();
    Ok(found)
}

fn walk(directory: &Path, found: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if path.file_name().is_some_and(|name| name == RECORD_FILE) {
            found.push(path.clone());
        }
        if file_type.is_dir() {
            walk(&path, found)?;
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
