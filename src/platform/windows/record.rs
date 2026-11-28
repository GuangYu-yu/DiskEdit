//! 记录位置上的文件操作。
//!
//! 位置上的东西必须是普通文件：符号链接、目录与其他类型一律回答"不是恢复记录"，
//! 读、写、删、改名都不跟随链接。写入用同目录的临时名落盘后原子改名，半成品不会
//! 以正式名出现。本平台没有目录项落盘的原语，删除与改名之后不宣称目录项已持久化。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::platform::{PlatformError, PlatformResult, RecordEntry};

/// 读取记录位置；不是普通文件时给出明确结论，只有真读不到才是错误。
pub fn read(path: &Path) -> PlatformResult<RecordEntry> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RecordEntry::Absent),
        Err(error) => return Err(PlatformError::io(path, error)),
    };
    if !metadata.is_file() {
        return Ok(RecordEntry::NotARecoveryFile);
    }
    let mut file = File::open(path).map_err(|error| PlatformError::io(path, error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| PlatformError::io(path, error))?;
    Ok(RecordEntry::Contents(bytes))
}

/// 以覆盖方式写记录位置；同目录临时名落盘后原子改名。
pub fn write(path: &Path, bytes: &[u8]) -> PlatformResult<()> {
    let directory = parent_of(path)?;
    let (temporary, mut file) = create_temporary(&directory)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| PlatformError::io(&temporary, error));
    drop(file);
    if let Err(error) = result {
        drop(std::fs::remove_file(&temporary));
        return Err(error);
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            drop(std::fs::remove_file(&temporary));
            Err(PlatformError::io(path, error))
        }
    }
}

/// 删除记录位置；不存在视为成功。
pub fn remove(path: &Path) -> PlatformResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PlatformError::io(path, error)),
    }
}

/// 在同一目录内改名；目标已存在时原子替换。
pub fn rename(from: &Path, to: &Path) -> PlatformResult<()> {
    std::fs::rename(from, to).map_err(|error| PlatformError::io(from, error))
}

/// 递归列出记录目录里指定文件名的条目，不进入符号链接目录。
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
        if path.file_name().is_some_and(|entry| entry == name) {
            found.push(path.clone());
        }
        // `DirEntry::file_type` 不跟随符号链接：链接目录不会被递归进入。
        if entry.file_type()?.is_dir() {
            walk(&path, name, found)?;
        }
    }
    Ok(())
}

fn parent_of(path: &Path) -> PlatformResult<PathBuf> {
    match path.parent() {
        Some(parent) => Ok(parent.to_path_buf()),
        None => Err(PlatformError::parse("a record path has no parent")),
    }
}

/// 在同目录建立一个新名字的临时文件；`create_new` 保证不与已有文件冲突。
fn create_temporary(directory: &Path) -> PlatformResult<(PathBuf, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    loop {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!(".record.{}.{serial}.tmp", std::process::id());
        let path = directory.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(PlatformError::io(&path, error)),
        }
    }
}
