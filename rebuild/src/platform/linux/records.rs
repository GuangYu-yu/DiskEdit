//! 记录目录：按目标身份派生目录，整份写入、整份读出。
//!
//! 目录与文件都走私有打开方式：目录 0700，文件 0600，拒绝符号链接与多硬链接。
//! 写入是"暂存 + 提交 + 目录同步"，因此读到的要么是上一份完整记录，要么是新一份，
//! 不会读到半份字节。

use std::path::{Path, PathBuf};

use super::source::guarded;
use crate::platform::{PlatformError, PlatformResult, RecordEntry, RecordStore, TargetKind};

/// 记录文件名。
const RECORD_NAME: &str = "record.bin";
/// 归档文件名的前缀。
const ARCHIVE_PREFIX: &str = "record.";
/// 归档文件名的后缀。
const ARCHIVE_SUFFIX: &str = ".abandoned";
/// 状态目录环境变量。
const STATE_DIR_ENV: &str = "DISKEDIT_STATE_DIR";

/// Linux 的记录目录。
#[derive(Debug)]
pub struct LinuxRecords {
    dir: PathBuf,
}

impl LinuxRecords {
    /// 由目标路径与身份构造；目录尚不存在时在首次写入时创建。
    pub fn new(_target: &Path, kind: TargetKind, identity: &[u8; 32]) -> Self {
        let root = state_root(kind);
        let key = hex16(identity);
        Self {
            dir: root.join("recovery").join(key),
        }
    }

    fn archived_name(&self, index: u32) -> String {
        format!("{ARCHIVE_PREFIX}{index}{ARCHIVE_SUFFIX}")
    }
}

impl RecordStore for LinuxRecords {
    fn location(&self) -> &Path {
        &self.dir
    }

    fn load(&self) -> PlatformResult<Option<Vec<u8>>> {
        let path = self.dir.join(RECORD_NAME);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PlatformError::io(&path, error)),
        }
        let mut file = guarded::open_private_file(&path, false)?;
        let mut bytes = Vec::new();
        use std::io::Read;
        file.read_to_end(&mut bytes)
            .map_err(|error| PlatformError::io(&path, error))?;
        Ok(Some(bytes))
    }

    fn store(&mut self, bytes: &[u8]) -> PlatformResult<()> {
        guarded::create_private_dir(&self.dir)?;
        guarded::write_atomic(&self.dir, RECORD_NAME, bytes)
    }

    fn clear(&mut self) -> PlatformResult<()> {
        let path = self.dir.join(RECORD_NAME);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PlatformError::io(&path, error)),
        }
    }

    fn entries(&self) -> PlatformResult<Vec<RecordEntry>> {
        let handle = match std::fs::read_dir(&self.dir) {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PlatformError::io(&self.dir, error)),
        };
        let mut entries = Vec::new();
        for item in handle {
            let item = item.map_err(|error| PlatformError::io(&self.dir, error))?;
            let file_type = item
                .file_type()
                .map_err(|error| PlatformError::io(item.path(), error))?;
            if !file_type.is_file() {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let bytes = item
                .metadata()
                .map_err(|error| PlatformError::io(item.path(), error))?
                .len();
            entries.push(RecordEntry {
                active: name == RECORD_NAME,
                name,
                bytes,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn quarantine(&mut self) -> PlatformResult<String> {
        let active = self.dir.join(RECORD_NAME);
        if std::fs::symlink_metadata(&active).is_err() {
            return Err(PlatformError::not_supported("没有可归档的记录"));
        }
        let mut index = 0u32;
        loop {
            let name = self.archived_name(index);
            let path = self.dir.join(&name);
            if std::fs::symlink_metadata(&path).is_err() {
                std::fs::rename(&active, &path).map_err(|error| PlatformError::io(&path, error))?;
                return Ok(name);
            }
            index = index
                .checked_add(1)
                .ok_or_else(|| PlatformError::unsupported("归档文件名已用尽"))?;
        }
    }
}

/// 状态目录：显式变量优先；块设备落在系统状态目录，普通文件落在用户状态目录。
fn state_root(kind: TargetKind) -> PathBuf {
    if let Some(value) = std::env::var_os(STATE_DIR_ENV) {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    match kind {
        TargetKind::Block => PathBuf::from("/var/lib/diskedit"),
        TargetKind::File => user_state_dir().join("diskedit"),
    }
}

fn user_state_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(".local/state"),
        _ => PathBuf::from("."),
    }
}

fn hex16(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in &bytes[..16] {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}
