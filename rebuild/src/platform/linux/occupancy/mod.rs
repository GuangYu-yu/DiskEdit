//! 占用视图：挂载表、交换区、holders、阵列成员与 loop 后备。

pub mod arrays;
pub mod holders;
pub mod loops;
pub mod mounts;
pub mod swap;

use crate::platform::{MountEntry, Occupancy, PlatformResult, SwapEntry};

use super::target::{disk_name, Target};

/// 汇总目标作用域内的占用证据。
///
/// 块设备的作用域是自身与全部分区：挂载与交换区按设备号匹配，holders 与阵列
/// 成员按内核设备名判定。文件的作用域只有自身：loop 后备文件指向它即占用。
pub fn probe(target: &Target) -> PlatformResult<Occupancy> {
    let ids = device_ids(target)?;
    let mounts: Vec<MountEntry> = mounts::read()?
        .into_iter()
        .filter(|entry| ids.iter().any(|(major, minor)| entry.major == *major && entry.minor == *minor))
        .collect();
    let swaps: Vec<SwapEntry> = swap::read()?
        .into_iter()
        .filter(|entry| same_path(&entry.path, target.path()))
        .collect();

    let (holders, array_member) = if target.is_block() {
        let kname = disk_name(target.path())?;
        (holders::read(&kname)?, arrays::member(&kname)?)
    } else {
        (Vec::new(), false)
    };
    let loops = loops::referencing(target.path())?
        .into_iter()
        .map(|use_| use_.device)
        .collect();

    Ok(Occupancy {
        mounts,
        swaps,
        holders,
        array_member,
        loops,
    })
}

/// 目标与全部分区的设备号。
fn device_ids(target: &Target) -> PlatformResult<Vec<(u32, u32)>> {
    if !target.is_block() {
        return Ok(Vec::new());
    }
    let kname = disk_name(target.path())?;
    let base = super::super::sysfs_root().join("block").join(&kname);
    let mut ids = Vec::new();
    if let Some(text) = super::super::read_optional_attr(&base.join("dev"))? {
        ids.push(parse_dev(&text)?);
    }
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
        Err(error) => return Err(crate::platform::PlatformError::io(&base, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| crate::platform::PlatformError::io(&base, error))?;
        let path = entry.path();
        if super::super::read_optional_attr(&path.join("partition"))?.is_none() {
            continue;
        }
        if let Some(text) = super::super::read_optional_attr(&path.join("dev"))? {
            ids.push(parse_dev(&text)?);
        }
    }
    Ok(ids)
}

fn parse_dev(text: &str) -> PlatformResult<(u32, u32)> {
    let (major, minor) = text
        .trim()
        .split_once(':')
        .ok_or_else(|| crate::platform::PlatformError::parse(format!("设备号缺少冒号：{text:?}")))?;
    let major = major
        .parse::<u32>()
        .map_err(|_| crate::platform::PlatformError::parse(format!("设备号主号无效：{text:?}")))?;
    let minor = minor
        .parse::<u32>()
        .map_err(|_| crate::platform::PlatformError::parse(format!("设备号次号无效：{text:?}")))?;
    Ok((major, minor))
}

/// 两个路径是否指向同一对象。
pub fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
