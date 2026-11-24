//! 挂载点反查：把用户给的目录还原成「承载它的整盘」与「分区号」。
//!
//! 依据 `/proc/self/mountinfo` 找到覆盖该目录的最深挂载点与设备号，再经
//! `/sys/dev/block/<major>:<minor>` 解析出内核设备名。只有落在某个分区上的挂载点
//! 才能反查；整盘挂载、LVM/dm 挂载或未挂载目录一律如实失败，不用整盘冒充分区。

use std::path::{Component, Path, PathBuf};

use crate::platform::{MountEntry, MountLocation, PlatformError, PlatformResult};

use super::{occupancy, read_optional_attr, sysfs_root};

/// 反查挂载点所在的整盘目标与分区号。
pub fn resolve(mount_point: &Path) -> PlatformResult<MountLocation> {
    let canonical = canonical_mount_point(mount_point)?;
    let entries = occupancy::mounts::read()?;
    let entry = select(&entries, &canonical)?;
    let kname = kernel_name(entry.major, entry.minor)?;
    let class = sysfs_root().join("class/block").join(&kname);
    let partition_text = read_optional_attr(&class.join("partition"))?.ok_or_else(|| {
        PlatformError::parse(format!(
            "mount point {} device {kname} is not a partition; whole-disk or logical volume mounts are not supported in this form",
            canonical.display()
        ))
    })?;
    let partition = partition_text.trim().parse::<u32>().map_err(|_| {
        PlatformError::parse(format!(
            "invalid partition number for {kname}: {partition_text:?}"
        ))
    })?;
    let disk = disk_name(&class)?;
    Ok(MountLocation {
        target: Path::new("/dev").join(disk),
        partition,
    })
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

/// 选出覆盖 `path` 的最深挂载点；同一挂载点被叠挂时取最后一条（最上层）。
fn select<'a>(entries: &'a [MountEntry], path: &Path) -> PlatformResult<&'a MountEntry> {
    let mut best: Option<(usize, &MountEntry)> = None;
    for entry in entries {
        let depth = match is_prefix(&entry.mount_point, path) {
            true => entry.mount_point.components().count(),
            false => continue,
        };
        if best.is_none_or(|(current, _)| depth >= current) {
            best = Some((depth, entry));
        }
    }
    best.map(|(_, entry)| entry).ok_or_else(|| {
        PlatformError::parse(format!("{} is not under any mount point", path.display()))
    })
}

/// `prefix` 是否为 `path` 的路径前缀（按分量比较，`/mnt` 不匹配 `/mnt2`）。
fn is_prefix(prefix: &Path, path: &Path) -> bool {
    let mut left = prefix.components();
    let mut right = path.components();
    loop {
        match (left.next(), right.next()) {
            (None, _) => return true,
            (Some(Component::RootDir), Some(Component::RootDir)) => continue,
            (Some(a), Some(b)) if a == b => continue,
            _ => return false,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(device: &str, mount_point: &str) -> MountEntry {
        let (major, minor) = device.split_once(':').expect("设备号");
        MountEntry {
            major: major.parse().expect("主号"),
            minor: minor.parse().expect("次号"),
            mount_point: PathBuf::from(mount_point),
        }
    }

    #[test]
    fn longest_prefix_wins() {
        let entries = [
            entry("8:1", "/"),
            entry("8:2", "/home"),
            entry("8:3", "/home/data"),
        ];
        let chosen = select(&entries, Path::new("/home/data/file")).expect("可选中");
        assert_eq!(chosen.device(), "8:3");
        let chosen = select(&entries, Path::new("/home/other")).expect("可选中");
        assert_eq!(chosen.device(), "8:2");
        let chosen = select(&entries, Path::new("/var")).expect("可选中");
        assert_eq!(chosen.device(), "8:1");
    }

    #[test]
    fn prefix_compares_by_component() {
        assert!(is_prefix(Path::new("/mnt"), Path::new("/mnt/a")));
        assert!(is_prefix(Path::new("/"), Path::new("/a")));
        assert!(!is_prefix(Path::new("/mnt"), Path::new("/mnt2")));
        assert!(!is_prefix(Path::new("/mnt/a"), Path::new("/mnt")));
    }

    #[test]
    fn overmounted_point_takes_the_last_entry() {
        let entries = [entry("8:1", "/srv"), entry("8:2", "/srv")];
        let chosen = select(&entries, Path::new("/srv/x")).expect("可选中");
        assert_eq!(chosen.device(), "8:2");
    }

    #[test]
    fn unmounted_path_is_refused() {
        let entries = [entry("8:1", "/home")];
        assert!(select(&entries, Path::new("/srv")).is_err());
    }
}
