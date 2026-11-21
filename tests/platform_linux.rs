#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程 / 设备）
//! Linux 平台端口的行为。
//!
//! 非 Linux 的能力门结果（只读镜像放行 / 写与证据命令退 6）由
//! `tests/capability_matrix.rs` 覆盖；本文件只在
//! Linux 上编译与运行。
//!
//! 需要真机/root/外部工具（`losetup`）的用例在运行期探测，条件不满足时跳过，不使用
//! `#[ignore]`。**跳过必须可闻**：走 [`capability_skip`]，它打一行说明，并在
//! `DISKEDIT_REQUIRE_CAPABILITIES` 置位时直接失败——静默 `return` 会把"这条断言没跑"
//! 伪装成"通过"

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// 能力探测失败的唯一出口。没有这条通道时，跳过与通过在测试报告里长得一模一样；
/// 而声明了"这台机器可测"的环境里再静默跳过，就是测试自己出了问题
fn capability_skip(what: &str, why: &str) {
    assert!(
        std::env::var_os("DISKEDIT_REQUIRE_CAPABILITIES").is_none(),
        "{what} could not be exercised: {why} — this environment declared the capability, \
         so a silent skip is a bug in the test or the environment"
    );
    eprintln!("note: skipping {what}: {why}");
}

/// 本次测试进程的镜像状态根：子进程与测试算出的落点必须一致
fn state_root() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("diskedit_platform_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        private_dir(&dir);
        dir
    })
}

/// 记录仓库的每一级都按 0700 建立：写命令会复核状态根、`images` 与键目录
fn private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).expect("create the state root");
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .env("DISKEDIT_STATE_DIR", state_root())
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
        capability_skip(
            "a block-device target",
            "losetup did not yield a loop device (needs root and a free loop slot)",
        );
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
