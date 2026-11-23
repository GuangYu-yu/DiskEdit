//! 挂载参数构造。
//!
//! 只拼参数，不执行程序；执行经 [`crate::platform::Platform::run`]。

use std::path::Path;

/// 构造 `mount` 的参数。
pub fn mount_args(
    source: &Path,
    mount_point: &Path,
    fstype: Option<&str>,
    options: &[&str],
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(fstype) = fstype {
        args.push("-t".to_string());
        args.push(fstype.to_string());
    }
    if !options.is_empty() {
        args.push("-o".to_string());
        args.push(options.join(","));
    }
    args.push(source.to_string_lossy().into_owned());
    args.push(mount_point.to_string_lossy().into_owned());
    args
}

/// 构造 `umount` 的参数。
pub fn umount_args(mount_point: &Path) -> Vec<String> {
    vec![mount_point.to_string_lossy().into_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_mount_arguments_in_order() {
        let args = mount_args(
            Path::new("/dev/sda1"),
            Path::new("/mnt/x"),
            Some("ext4"),
            &["ro", "noatime"],
        );
        assert_eq!(
            args,
            vec!["-t", "ext4", "-o", "ro,noatime", "/dev/sda1", "/mnt/x"]
        );
    }

    #[test]
    fn omits_optional_flags_when_absent() {
        let args = mount_args(Path::new("/a.img"), Path::new("/mnt/y"), None, &[]);
        assert_eq!(args, vec!["/a.img", "/mnt/y"]);
        assert_eq!(umount_args(Path::new("/mnt/y")), vec!["/mnt/y"]);
    }
}
