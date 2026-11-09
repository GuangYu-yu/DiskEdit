//! 记录目录的属主与权限：块设备侧逐级 `0700` 且属主为 root，镜像侧 `0700`
//! 且与目标文件同主。这里只覆盖镜像形态——它不需要真实设备；块设备形态
//! 的核对在创建函数里做（uid 0 与 `mode & 0o022 == 0`），要在 root 下才能
//! 触达，留给 Linux 全量。
//!
//! 非 Windows 平台才创建 sidecar 目录；其余平台提前返回。

#![cfg(unix)]

use std::process::Command;

#[cfg(target_os = "linux")]
#[test]
fn the_sidecar_directory_is_private_to_the_owner() {
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let dir = tempfile::Builder::new()
        .prefix("diskedit-perm-")
        .tempdir()
        .unwrap();
    let img = dir.path().join("disk.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();

    let out = Command::new(exe)
        .args(["new", img.to_str().unwrap(), "--yes"])
        .output()
        .unwrap();
    assert!(out.status.success(), "a blank image takes `new` cleanly");

    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let sidecar = dir.path().join("disk.img.diskedit");
    let meta = std::fs::symlink_metadata(&sidecar)
        .unwrap_or_else(|e| panic!("the record directory must exist beside the image: {e}"));
    assert!(meta.is_dir(), "the record location is a directory, not a file");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "the record directory must be private to its owner"
    );
    let img_meta = std::fs::metadata(&img).unwrap();
    assert_eq!(
        (meta.uid(), meta.gid()),
        (img_meta.uid(), img_meta.gid()),
        "the record directory must belong to the image's owner"
    );
}
