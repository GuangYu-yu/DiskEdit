//! 阵列成员视图：检查 `/sys/block/md*/md/dev-<kname>` 是否存在。

use crate::platform::{PlatformError, PlatformResult};

/// 设备是否为任一 md 阵列的成员。
pub fn member(kname: &str) -> PlatformResult<bool> {
    let block_dir = super::super::sysfs_root().join("block");
    let entries = match std::fs::read_dir(&block_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(PlatformError::io(&block_dir, error)),
    };
    let marker = format!("dev-{kname}");
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(&block_dir, error))?;
        let name = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };
        if !name.starts_with("md") {
            continue;
        }
        if entry.path().join("md").join(&marker).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}
