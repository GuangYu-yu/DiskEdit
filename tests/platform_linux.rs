#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程 / 设备）
//! Linux 平台端口的行为。
//!
//! 非 Linux 的能力门结果（只读镜像放行 / 写与证据命令退 6）由
//! `tests/capability_matrix.rs` 覆盖；本文件只在
//! Linux 上编译与运行。
//!
//! 需要真机/root/外部工具（`losetup`）的用例在运行期探测，条件不满足时提前返回，
//! 不使用 `#[ignore]`。

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit-platform-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn image_with(dir: &Path, name: &str, size: u64) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, vec![0u8; size as usize]).expect("create image");
    path
}

#[test]
fn info_reports_the_geometry_and_table_fields() {
    let dir = temp_dir("info");
    let img = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let p = img.to_str().unwrap();
    let (code, _, err) = run(&["new", p, "--table", "gpt", "--yes"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run(&["info", p]);
    assert_eq!(code, 0, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("info emits one JSON object");
    assert_eq!(json["label"], "gpt", "{out}");
    assert_eq!(json["sector_size"], 512, "{out}");
    assert_eq!(json["size_bytes"], 8 * 1024 * 1024, "{out}");
    assert!(json["partitions"].is_array(), "{out}");
}

#[test]
fn an_image_honours_the_declared_sector_size() {
    let dir = temp_dir("sector-size");
    let img = image_with(&dir, "disk.img", 16 * 1024 * 1024);
    let p = img.to_str().unwrap();
    let (code, _, err) = run(&["new", p, "--table", "gpt", "--sector-size", "4096", "--yes"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run(&["info", p, "--sector-size", "4096"]);
    assert_eq!(code, 0, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("info emits one JSON object");
    assert_eq!(json["sector_size"], 4096, "{out}");
}

#[test]
fn the_linux_port_runs_the_table_write_commands() {
    let dir = temp_dir("write");
    let img = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let p = img.to_str().unwrap();

    let (code, _, err) = run(&["new", p, "--table", "gpt", "--yes"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run(&["add", p, "1M", "--yes"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run(&["delete", &format!("{p}:1"), "--yes"]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn a_block_device_refuses_the_sector_size_override() {
    let dir = temp_dir("loop");
    let img = image_with(&dir, "backing.img", 8 * 1024 * 1024);
    let Some(device) = attach_loop(&img) else {
        return;
    };
    let (code, _, err) = run(&["info", &device, "--sector-size", "4096"]);
    detach_loop(&device);
    assert_eq!(code, 3, "a block device must refuse --sector-size: {err}");
}

fn attach_loop(image: &Path) -> Option<String> {
    let out = Command::new("losetup")
        .args(["--find", "--show"])
        .arg(image)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let device = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!device.is_empty()).then_some(device)
}

fn detach_loop(device: &str) {
    let _ = Command::new("losetup").args(["--detach", device]).status();
}
