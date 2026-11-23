//! 设备持有者：递归读取 `/sys/block/<kname>/holders/`。
//!
//! holders 目录里的每一项是一个正在使用该设备的上层设备。递归展开覆盖多层映射；
//! 深度上限 8，超过时报告错误而不是截断，避免把"看不全"说成"没有占用"。

use std::path::Path;

use crate::platform::{PlatformError, PlatformResult};

const MAX_DEPTH: usize = 8;

/// 读取设备的持有者集合，含间接持有者。
pub fn read(kname: &str) -> PlatformResult<Vec<String>> {
    let mut found = Vec::new();
    visit(kname, 0, &mut found)?;
    Ok(found)
}

fn visit(kname: &str, depth: usize, found: &mut Vec<String>) -> PlatformResult<()> {
    if depth > MAX_DEPTH {
        return Err(PlatformError::unsupported(format!(
            "{kname} 的 holders 链超过深度上限 {MAX_DEPTH}"
        )));
    }
    let dir = super::super::sysfs_root()
        .join("block")
        .join(kname)
        .join("holders");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PlatformError::io(&dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(&dir, error))?;
        let holder = file_name(&entry.path())?;
        if found.iter().any(|seen| seen == &holder) {
            continue;
        }
        found.push(holder.clone());
        visit(&holder, depth + 1, found)?;
    }
    Ok(())
}

fn file_name(path: &Path) -> PlatformResult<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PlatformError::parse(format!("{} 中没有设备名", path.display())))
}
