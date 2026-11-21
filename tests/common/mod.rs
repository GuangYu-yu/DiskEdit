//! VM 载体用例共用的环境闸门与进程工具。
//!
//! 三个用例文件都以 `mod common;` 引入本目录（`tests/common/` 是目录，不是独立 target）。
//! 它们要回答的是同一件事——"这台机器够不够格跑载体用例"——所以答案只写一份：
//! 缺环境时打印补验命令后跳过；`DISKEDIT_REQUIRE_CAPABILITIES` 置位时跳过即失败。
//!
//! `dead_code` 放行的理由：每个测试 crate 只用其中一部分，某个入口在本 crate 里没有
//! 调用点，却在另一个 crate 里是主路径。

#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Stdio};

/// 被遵从的产物：一个带 `DISKEDIT_STATE_DIR`、stdin 关掉的 CLI 调用
pub fn disk_edit() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
    command.stdin(Stdio::null());
    command
}

/// 跑完一整轮，收齐 stdout/stderr。`state` 是这台机器上的状态根
pub fn run(state: &Path, args: &[&str]) -> (i32, String, String) {
    let out = disk_edit()
        .args(args)
        .env("DISKEDIT_STATE_DIR", state)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 只有自己能进去的目录：状态根与镜像都不该被同机的其他用户碰到
pub fn private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).expect("create the directory");
}

/// 缺失环境时的唯一出口：跳过必须可闻，并写出补验方式
pub fn skip(target: &str, what: &str, why: &str) {
    eprintln!(
        "note: not executed {what}: {why}\n  verify with: DISKEDIT_VM_TESTS=1 \
         DISKEDIT_VM_BUNDLE=<bundle> cargo test --test {target} -- --test-threads=1"
    );
    assert!(
        std::env::var_os("DISKEDIT_REQUIRE_CAPABILITIES").is_none(),
        "{what} could not be exercised: {why} — this environment declared the capability, \
         so a silent skip is a bug in the test or the environment"
    );
}

/// 载体用例的统一前置：显式开启、有 bundle、有 qemu。三者缺一即跳过
pub fn gate(target: &str, what: &str) -> bool {
    if std::env::var_os("DISKEDIT_VM_TESTS").is_none() {
        skip(target, what, "the carrier tests are opt-in: they need a bundle and QEMU");
        return false;
    }
    if std::env::var_os("DISKEDIT_VM_BUNDLE").is_none() {
        skip(target, what, "DISKEDIT_VM_BUNDLE is not set, so placement cannot pick the VM carrier");
        return false;
    }
    if !Command::new("sh")
        .args(["-c", "command -v qemu-system-x86_64 || command -v qemu-system-aarch64"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        skip(target, what, "no qemu-system binary is on PATH");
        return false;
    }
    true
}

/// CLI 直接拉起 QEMU，所以载体就是 CLI 的子进程
pub fn qemu_child_of(cli: u32) -> Option<u32> {
    let out = Command::new("pgrep").args(["-P", &cli.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

/// 全机的 QEMU 进程：用来判"会话结束后有没有留下孤儿"
pub fn qemu_pids() -> Vec<u32> {
    let out = Command::new("pgrep").args(["-a", "qemu-system"]).output().expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

pub fn interrupt(pid: u32, signal: libc::c_int) {
    // SAFETY: 只向本用例自己拉起的进程发一个信号；失败（已退出）由调用方的等待覆盖
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// 等一个事件，超时即失败。等待窗口由调用方给出，不写死在 helpers 里
pub fn wait_until(limit: std::time::Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    ready()
}
