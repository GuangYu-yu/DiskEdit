//! 挂载事实：把一个目录还原成承载它的块设备与分区身份。
//!
//! 依据 `/proc/self/mountinfo` 的挂载点与设备号，再经 `/sys/class/block` 读取分区
//! 属性。未挂载、多重挂载、非块设备与整盘/逻辑卷挂载都作为事实返回，由上层决定
//! 拒绝与否；这里不猜设备，也不把"无法映射"折叠成"没有挂载"。
//!
//! btrfs 的挂载设备号是匿名设备号，不能直接映射到 sysfs；它的底层设备经文件系统
//! 接口的 FSID 与 `/sys/fs/btrfs/<uuid>/devices/` 反查，且只接受单设备文件系统。

use std::path::{Path, PathBuf};

use crate::platform::{
    MountEntry, MountFacts, PartitionRef, Platform, PlatformError, PlatformResult,
};

use super::{Linux, kernel, occupancy, read_optional_attr, sysfs_root};

/// 反查目录的挂载事实。
pub fn resolve(platform: &Linux, mount_point: &Path) -> PlatformResult<MountFacts> {
    let canonical = canonical_mount_point(mount_point)?;
    let entries = occupancy::mounts::read()?;
    let matched: Vec<&MountEntry> = entries
        .iter()
        .filter(|entry| entry.mount_point == canonical || entry.mount_point == mount_point)
        .collect();
    match matched.as_slice() {
        [] => Ok(MountFacts::NotMounted),
        [entry] => mounted(platform, &canonical, entry),
        _ => Ok(MountFacts::Ambiguous),
    }
}

fn mounted(platform: &Linux, canonical: &Path, entry: &MountEntry) -> PlatformResult<MountFacts> {
    let name = if entry.fstype == "btrfs" {
        match btrfs_device(platform, canonical)? {
            Some(name) => name,
            None => {
                return Ok(MountFacts::Unmapped {
                    reason: format!(
                        "the btrfs filesystem at {} spans more than one device",
                        canonical.display()
                    ),
                });
            }
        }
    } else if entry.major == 0 {
        return Ok(MountFacts::NotBlockDevice {
            source: entry.source.clone(),
        });
    } else {
        kernel_name(entry.major, entry.minor)?
    };
    let class = sysfs_root().join("class/block").join(&name);
    let device = PathBuf::from("/dev").join(&name);
    let Some(number) = read_optional_attr(&class.join("partition"))? else {
        return Ok(MountFacts::NotPartition { device });
    };
    let number = number.trim().parse::<u32>().map_err(|_| {
        PlatformError::parse(format!("invalid partition number for {name}: {number:?}"))
    })?;
    let sectors = read_attr_u64(&class.join("size"))?;
    let disk = disk_name(&class)?;
    Ok(MountFacts::Mounted {
        device,
        partition: Some(PartitionRef {
            disk: PathBuf::from("/dev").join(disk),
            number,
            sectors,
        }),
    })
}

/// btrfs 文件系统登记的单块设备名；跨多设备时返回 `None`。
fn btrfs_device(platform: &Linux, mount_point: &Path) -> PlatformResult<Option<String>> {
    let show = platform.btrfs_show(mount_point)?;
    if show.devices != 1 {
        return Ok(None);
    }
    let dir = sysfs_root()
        .join("fs/btrfs")
        .join(kernel::fsid_uuid(&show.fsid))
        .join("devices");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|error| PlatformError::io(&dir, error))? {
        let entry = entry.map_err(|error| PlatformError::io(&dir, error))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    match names.as_slice() {
        [name] => Ok(Some(name.clone())),
        _ => Ok(None),
    }
}

/// 目录必须存在才能反查；不存在属于请求与现场不符。
fn canonical_mount_point(mount_point: &Path) -> PlatformResult<PathBuf> {
    std::fs::canonicalize(mount_point).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PlatformError::parse(format!(
                "mount point {} does not exist",
                mount_point.display()
            ))
        } else {
            PlatformError::io(mount_point, error)
        }
    })
}

/// 由设备号解析内核设备名（如 `sda1`、`nvme0n1p1`）。
fn kernel_name(major: u32, minor: u32) -> PlatformResult<String> {
    let link = sysfs_root()
        .join("dev/block")
        .join(format!("{major}:{minor}"));
    let resolved = std::fs::canonicalize(&link).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PlatformError::parse(format!("device {major}:{minor} does not exist in sysfs"))
        } else {
            PlatformError::io(&link, error)
        }
    })?;
    resolved
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            PlatformError::parse(format!("device {major}:{minor} has no kernel device name"))
        })
}

/// 分区所在整盘的内核设备名：`class/block/<分区名>` 的真实路径的父目录名。
fn disk_name(partition_class: &Path) -> PlatformResult<String> {
    let resolved = std::fs::canonicalize(partition_class)
        .map_err(|error| PlatformError::io(partition_class, error))?;
    resolved
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            PlatformError::parse(format!(
                "parent directory of {} is not a whole-disk device directory",
                partition_class.display()
            ))
        })
}

fn read_attr_u64(path: &Path) -> PlatformResult<u64> {
    let text = std::fs::read_to_string(path).map_err(|error| PlatformError::io(path, error))?;
    text.trim().parse::<u64>().map_err(|_| {
        PlatformError::parse(format!(
            "{} does not contain an unsigned integer",
            path.display()
        ))
    })
}

/// 给定设备是否为活动交换区。
pub fn swap_active(device: &Path) -> PlatformResult<bool> {
    let canonical = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    for entry in occupancy::swap::read()? {
        let path = std::fs::canonicalize(&entry.path).unwrap_or(entry.path);
        if path == canonical {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(device: &str, mount_point: &str, fstype: &str) -> MountEntry {
        let (major, minor) = device.split_once(':').expect("设备号");
        MountEntry {
            major: major.parse().expect("主号"),
            minor: minor.parse().expect("次号"),
            mount_point: PathBuf::from(mount_point),
            fstype: fstype.to_string(),
            source: PathBuf::from("/dev/x"),
        }
    }

    #[test]
    fn selects_only_the_exact_mount_point() {
        let entries = [entry("8:1", "/home", "ext4")];
        let matched: Vec<&MountEntry> = entries
            .iter()
            .filter(|entry| entry.mount_point == Path::new("/home"))
            .collect();
        assert_eq!(matched.len(), 1);
        let inside: Vec<&MountEntry> = entries
            .iter()
            .filter(|entry| entry.mount_point == Path::new("/home/data"))
            .collect();
        assert!(inside.is_empty());
    }

    #[test]
    fn overmounted_point_is_ambiguous() {
        let entries = [entry("8:1", "/srv", "ext4"), entry("8:2", "/srv", "xfs")];
        let matched: Vec<&MountEntry> = entries
            .iter()
            .filter(|entry| entry.mount_point == Path::new("/srv"))
            .collect();
        assert_eq!(matched.len(), 2);
    }
}
