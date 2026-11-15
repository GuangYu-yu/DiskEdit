//! 记录目录的属主与权限：块设备侧逐级 `0700` 且属主为 root，镜像侧 `0700`
//! 且与目标文件同主。这里只覆盖镜像形态——它不需要真实设备；块设备形态
//! 的核对在创建函数里做（uid 0 与 `mode & 0o077 == 0`），要在 root 下才能
//! 触达，留给 Linux 全量。
//!
//! 本文件只在 Linux 上编译与运行：sidecar 目录由 Linux 实现创建，其余平台没有
//! 这条路径（`#![cfg(unix)]` 挡掉非 Unix，用例本身另按 `target_os = "linux"` 门控）。

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

/// 已存在的记录目录带 group/other 任何权限即拒绝：记录里有写前的分区表，
/// 同机其他账户可读即可复原盘的原始布局。拒绝必须是整体拒绝——目标一个字节都不改
#[cfg(target_os = "linux")]
#[test]
fn a_preexisting_group_accessible_sidecar_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let dir = tempfile::Builder::new()
        .prefix("diskedit-perm-")
        .tempdir()
        .unwrap();
    let img = dir.path().join("disk.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let sidecar = dir.path().join("disk.img.diskedit");
    std::fs::create_dir(&sidecar).unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new(exe)
        .args(["new", img.to_str().unwrap(), "--yes"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(3), "an accessible record directory is a refusal, not a silent fix");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chmod 700"), "the refusal must say how to fix it: {err}");
    let after = std::fs::symlink_metadata(&sidecar).unwrap();
    assert_eq!(
        after.permissions().mode() & 0o777,
        0o755,
        "the tool must not change the user's directory permissions"
    );
    assert!(
        std::fs::read(&img).unwrap().iter().all(|b| *b == 0),
        "the refusal happens before the first target write"
    );
}
