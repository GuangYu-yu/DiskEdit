//! loop 设备视图：读取 `/sys/block/<kname>/loop/backing_file`。
//!
//! 单个 loop 设备直接给出其后备文件；判断某个镜像文件是否被 loop 占用时，遍历
//! `/sys/block/loop*` 逐一比较后备文件路径。

use std::path::{Path, PathBuf};

use super::super::{PlatformError, PlatformResult};
use super::mounts::same_path;

/// 一个 loop 设备及其后备文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopUse {
    /// loop 设备的内核名，例如 `loop0`。
    pub device: String,
    /// 后备文件路径，取自 sysfs。
    pub backing: PathBuf,
}

/// 读取单个 loop 设备的后备文件；不存在或无该属性时为 `None`。
pub fn backing(kname: &str) -> PlatformResult<Option<PathBuf>> {
    let path = super::super::sysfs_root()
        .join("block")
        .join(kname)
        .join("loop/backing_file");
    match super::super::read_optional_attr(&path)? {
        Some(text) if !text.is_empty() => Ok(Some(PathBuf::from(text))),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

/// 找出以后备文件指向 `target` 的所有 loop 设备。
pub fn referencing(target: &Path) -> PlatformResult<Vec<LoopUse>> {
    let block_dir = super::super::sysfs_root().join("block");
    let entries = match std::fs::read_dir(&block_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io(&block_dir, error)),
    };
    let mut uses = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(&block_dir, error))?;
        let path = entry.path();
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if !name.starts_with("loop") {
            continue;
        }
        if let Some(backing) = backing(name)? {
            if same_path(&backing, target) {
                uses.push(LoopUse {
                    device: name.to_string(),
                    backing,
                });
            }
        }
    }
    uses.sort_by(|left, right| left.device.cmp(&right.device));
    Ok(uses)
}
