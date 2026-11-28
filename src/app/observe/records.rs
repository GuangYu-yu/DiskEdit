//! 恢复记录观察：记录目录、记录文件与解码前的字节。
//!
//! 记录的字节格式由纯核解释；本模块只负责按目标身份定位目录、读字节，并把
//! "读不到"与"没有记录"分开。

use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;

use crate::platform::{Platform, PlatformError, PlatformResult, RecordEntry};

/// 记录目录的文件名。
pub const RECORD_FILE: &str = "record";

/// 状态根下存放记录目录的那一级名字；`<state_root>/images/<key>/record`。
pub const IMAGE_DIR: &str = "images";

/// 一个目标的记录状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordState {
    /// 记录目录；身份不可用或宿主没有状态目录时为 `None`。
    pub directory: Option<PathBuf>,
    /// 记录目录是否存在；锁定位置的存在性是 `records show` 的前置事实。
    pub directory_exists: bool,
    /// 记录文件路径。
    pub file: Option<PathBuf>,
    /// 记录文件字节；文件不存在时为 `None`。
    pub bytes: Option<Vec<u8>>,
    /// 无法定位或读取的原因。
    pub unavailable: Option<String>,
}

/// 按目标身份定位记录目录。
pub fn locate(
    platform: &dyn Platform,
    fingerprint: Option<Fingerprint>,
) -> PlatformResult<RecordState> {
    let Some(root) = platform.state_root() else {
        return Ok(RecordState {
            directory: None,
            directory_exists: false,
            file: None,
            bytes: None,
            unavailable: Some("this host has no state directory".to_owned()),
        });
    };
    let Some(fingerprint) = fingerprint else {
        return Ok(RecordState {
            directory: None,
            directory_exists: false,
            file: None,
            bytes: None,
            unavailable: Some("target identity could not be established".to_owned()),
        });
    };
    let directory = directory(&root, fingerprint);
    let directory_exists = platform.is_directory(&directory);
    let file = record_file(&directory);
    match platform.read_record(&file) {
        Ok(RecordEntry::Contents(bytes)) => Ok(RecordState {
            directory: Some(directory),
            directory_exists,
            file: Some(file),
            bytes: Some(bytes),
            unavailable: None,
        }),
        Ok(RecordEntry::Absent) => Ok(RecordState {
            directory: Some(directory),
            directory_exists,
            file: Some(file),
            bytes: None,
            unavailable: None,
        }),
        Ok(RecordEntry::NotARecoveryFile) => Ok(RecordState {
            directory: Some(directory),
            directory_exists,
            file: Some(file.clone()),
            bytes: None,
            unavailable: Some(format!("{} is not a regular file", file.display())),
        }),
        Err(error) => {
            let message = format!("cannot read {}: {error}", file.display());
            Ok(RecordState {
                directory: Some(directory),
                directory_exists,
                file: Some(file),
                bytes: None,
                unavailable: Some(message),
            })
        }
    }
}

/// 读记录字节：不存在为 `None`，不是普通文件为错误。
///
/// 执行器只经这里读记录；判断"没有记录"与"位置被动过"的差异不能靠调用方各自拼。
pub fn read_bytes(platform: &dyn Platform, path: &Path) -> PlatformResult<Option<Vec<u8>>> {
    match platform.read_record(path)? {
        RecordEntry::Absent => Ok(None),
        RecordEntry::Contents(bytes) => Ok(Some(bytes)),
        RecordEntry::NotARecoveryFile => Err(PlatformError::parse(format!(
            "{} is not a regular file",
            path.display()
        ))),
    }
}

/// 身份目录名：指纹前 16 字节的小写十六进制。
pub fn hex16(fingerprint: Fingerprint) -> String {
    let bytes = fingerprint.bytes();
    let mut text = String::with_capacity(32);
    for byte in &bytes[..16] {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// 记录目录：`<state_root>/images/<key>`。定位与写入都从这里取路径。
pub fn directory(root: &Path, fingerprint: Fingerprint) -> PathBuf {
    root.join(IMAGE_DIR).join(hex16(fingerprint))
}

/// 记录文件：记录目录下的 `record`。
pub fn record_file(directory: &Path) -> PathBuf {
    directory.join(RECORD_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_layout_is_one_definition() {
        let fingerprint = Fingerprint::strong([0x0a; 32]);
        let root = Path::new("/state");
        let directory = directory(root, fingerprint);
        assert_eq!(
            directory,
            Path::new("/state/images/0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a")
        );
        assert_eq!(record_file(&directory), directory.join("record"));
    }
}
