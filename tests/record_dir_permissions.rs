//! 记录目录的属主与权限：块设备侧逐级 `0700` 且属主为 root，镜像侧
//! `<image_state_root>/images/<键摘要>` 逐级 `0700` 且属主为当前用户。这里只覆盖
//! 镜像形态——它不需要真实设备；块设备形态的核对在创建函数里做
//! （uid 0 与 `mode & 0o077 == 0`），要在 root 下才能触达，留给 Linux 全量。
//!
//! 本文件只在 Linux 上编译与运行：记录后端由 Linux 实现创建，其余平台没有这条路径。

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// 每级都按 `0700` 建目录：与产品对状态根的形态要求一致
#[cfg(target_os = "linux")]
fn private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).unwrap();
}

/// 镜像键目录的落点：键取文件对象身份，目录名是键的 sha256 前 16 字节
#[cfg(target_os = "linux")]
fn record_dir_in(state: &Path, img: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(img).unwrap();
    let key = format!("inode:{}:{}", meta.dev(), meta.ino());
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    state.join("images").join(hex)
}

#[cfg(target_os = "linux")]
#[test]
fn the_image_record_directory_is_private_to_the_owner() {
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let dir = tempfile::Builder::new()
        .prefix("diskedit-perm-")
        .tempdir()
        .unwrap();
    let img = dir.path().join("disk.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let state = dir.path().join("state");
    private_dir(&state);

    let out = Command::new(exe)
        .args(["new", img.to_str().unwrap(), "--yes"])
        .env("DISKEDIT_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(out.status.success(), "a blank image takes `new` cleanly");

    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let spot = record_dir_in(&state, &img);
    let meta = std::fs::symlink_metadata(&spot)
        .unwrap_or_else(|e| panic!("the record directory must exist in the state root: {e}"));
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
fn a_preexisting_group_accessible_record_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let dir = tempfile::Builder::new()
        .prefix("diskedit-perm-")
        .tempdir()
        .unwrap();
    let img = dir.path().join("disk.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let state = dir.path().join("state");
    private_dir(&state);
    let spot = record_dir_in(&state, &img);
    private_dir(&spot);
    std::fs::set_permissions(&spot, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new(exe)
        .args(["new", img.to_str().unwrap(), "--yes"])
        .env("DISKEDIT_STATE_DIR", &state)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(3), "an accessible record directory is a refusal, not a silent fix");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chmod 700"), "the refusal must say how to fix it: {err}");
    let after = std::fs::symlink_metadata(&spot).unwrap();
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
