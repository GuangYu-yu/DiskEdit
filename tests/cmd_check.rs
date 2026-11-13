//! `check` 的进程级测试。
//!
//! 四码：工具退出 0 → 0；工具非零 → 3；工具不在 `PATH` 或路径不存在
//! → 6；已找到但启动失败 → 4。工具侧的非零、缺失、启动失败要替换工具表才能造
//! （`TOOL_PATH` 是编译期常量，见 `src/fsops/env.rs`），故由
//! `tests/wsl/check_exit_codes.sh` 覆盖；本文件覆盖不依赖替换工具表的部分。
//!
//! 本文件钉住：
//!   - 缺 `:N` 落 2：「缺参数」归 schema 解析失败（语法层失败，不进入 `RunOutcome`）
//!   - `:99` 不存在 → 3
//!   - 分区没有已知文件系统 → 6（`no check tool wired`）
//!   - 目标不存在 → 4（`MutationState::None` 的打开失败）
//!   - 只读零写：目标字节、所在目录条目、证据目录都不因 `check` 改变
//!   - 真 ext4 → 0 且零写；需要 root、`/dev/loop-control` 与 e2fsprogs，
//!     运行期探测，缺任一项时提前返回（`check` 要走 loop 才能把分区交给 e2fsck）

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const TYPE_GUID_LINUX_DATA: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";

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

/// 零填充镜像 + 一个分区（起点 2048，`--align mib`）。分区内容全零，
/// 即「没有已知文件系统」
fn fixture(tag: &str, size_mib: usize, part_size: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("diskedit_check_{tag}_"))
        .tempdir()
        .expect("create the fixture directory");
    let img = dir.path().join("c.img");
    fs::write(&img, vec![0u8; size_mib * 1024 * 1024]).expect("create the image");
    let img_s = img.to_str().expect("utf-8 fixture path");
    let (code, _, err) = run(&["new", img_s, "--yes"]);
    assert_eq!(code, 0, "fixture `new` must succeed: {err}");
    let (code, _, err) = run(&[
        "add",
        img_s,
        part_size,
        "--align",
        "mib",
        "--type-guid",
        TYPE_GUID_LINUX_DATA,
        "--yes",
    ]);
    assert_eq!(code, 0, "fixture `add` must succeed: {err}");
    (dir, img)
}

/// 记录目录落点：`<img>.diskedit`（镜像侧 sidecar）
fn record_dir(img: &Path) -> PathBuf {
    let mut name = img.as_os_str().to_owned();
    name.push(".diskedit");
    PathBuf::from(name)
}

/// 目录直接子项名（排序）：用来断言"没有新建任何伴随物"
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read the fixture directory")
        .map(|e| e.expect("directory entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn target(img: &Path, part: &str) -> String {
    format!("{}:{part}", img.to_str().expect("utf-8 fixture path"))
}

/// 缺 `:N` 在解析期结束：「缺参数」→ 2。`cmd_check` 里那句 refused 分支
/// 走不到，因为 CLI 在分派前已退出
#[test]
fn a_missing_partition_selector_is_a_usage_error() {
    let (dir, img) = fixture("selector", 32, "8M");
    let before = fs::read(&img).expect("read the image");
    let entries = listing(dir.path());

    let (code, _, err) = run(&["check", img.to_str().unwrap()]);
    assert_eq!(code, 2, "missing :N is a usage error: {err}");
    assert!(err.contains("`check` needs a partition"), "{err}");

    assert_eq!(fs::read(&img).unwrap(), before, "a usage error must not write the target");
    assert_eq!(listing(dir.path()), entries, "a usage error must not create sidecars");
}

/// 分区号落在空槽 → 3：数字选择器先过命中核验，再谈文件系统
#[test]
fn a_missing_partition_number_is_refused() {
    let (dir, img) = fixture("missing_part", 32, "8M");
    let before = fs::read(&img).unwrap();
    let entries = listing(dir.path());

    // GPT 固定 128 个表项：:99 是界内空槽（`>128` 在解析期落 2），
    // 空槽在运行期按"分区不存在"拒绝
    let (code, _, err) = run(&["check", &target(&img, "99")]);
    assert_eq!(code, 3, "partition 99 does not exist: {err}");
    assert!(err.contains("partition 99 is empty"), "{err}");

    assert_eq!(fs::read(&img).unwrap(), before, "a refusal must not write the target");
    assert_eq!(listing(dir.path()), entries, "a refusal must not create sidecars");
}

/// 分区没有已知文件系统 → 6「no check tool wired」。这一路径在 attaching loop
/// 之前返回，因此不需要 root
#[test]
fn a_partition_without_a_filesystem_has_no_check_tool() {
    let (dir, img) = fixture("unknown_fs", 32, "8M");
    let before = fs::read(&img).unwrap();
    let entries = listing(dir.path());

    let (code, _, err) = run(&["check", &target(&img, "1")]);
    assert_eq!(code, 6, "an unidentified filesystem has no tool: {err}");
    assert!(err.contains("check failed: no check tool wired for unknown"), "{err}");

    assert_eq!(fs::read(&img).unwrap(), before, "a capability gap must not write the target");
    assert_eq!(listing(dir.path()), entries, "a capability gap must not create sidecars");
}

/// 目标不存在 → 4：打开失败发生在任何写入之前
#[test]
fn an_absent_target_is_an_io_failure() {
    let (dir, _img) = fixture("absent", 32, "8M");
    let absent = dir.path().join("absent.img");
    let entries = listing(dir.path());

    let (code, _, err) = run(&["check", &target(&absent, "1")]);
    assert_eq!(code, 3, "an absent target is an unmet precondition: {err}");
    assert!(err.contains("open failed"), "{err}");

    assert!(!absent.exists(), "check must not create the target");
    assert_eq!(listing(dir.path()), entries, "a failed open must not create sidecars");
}

/// `check` 不写目标、不建临时文件、不 rename、不 fsync、不删除。
/// 三条可观察路径（3 / 6 / 4）前后，目标字节与所在目录条目必须逐一相同
#[test]
fn a_refused_check_writes_nothing() {
    let (dir, img) = fixture("zero_write", 32, "8M");
    let cases: [String; 3] = [
        target(&img, "99"),
        target(&img, "1"),
        target(&dir.path().join("absent.img"), "1"),
    ];

    let bytes = fs::read(&img).unwrap();
    let entries = listing(dir.path());
    for t in &cases {
        let (code, _, err) = run(&["check", t]);
        assert_ne!(code, 0, "this fixture must not pass a check: {err}");

        assert_eq!(fs::read(&img).unwrap(), bytes, "`check {t}` changed the target bytes");
        assert_eq!(listing(dir.path()), entries, "`check {t}` changed the directory");
    }

    let rd = record_dir(&img);
    if rd.exists() {
        let record_entries: Vec<String> = fs::read_dir(&rd)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !record_entries.iter().any(|n| n == "record"),
            "`check` must not open the evidence path: {record_entries:?}"
        );
    }
}

/// `check` 在 ext4 上的成功路径（0），并核对零写。需要 root（loop + e2fsck 以 root
/// 访问设备节点）与 e2fsprogs；缺任一项时提前返回，并在输出里写明缺什么。
///
/// 工具可用性按 `TOOL_PATH` 的默认目录表探测；以 `DISKEDIT_TOOL_PATH` 自定义构建的
/// 二进制若把工具放在别处，本用例会提前返回而不是误报
#[test]
fn a_successful_check_writes_nothing() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping a_successful_check_writes_nothing: needs euid 0 for loop + e2fsck");
        return;
    }
    if !Path::new("/dev/loop-control").exists() {
        eprintln!("skipping a_successful_check_writes_nothing: needs /dev/loop-control");
        return;
    }
    for tool in ["losetup", "mke2fs", "e2fsck"] {
        if !in_system_tool_path(tool) {
            eprintln!("skipping a_successful_check_writes_nothing: {tool} not in the system tool path");
            return;
        }
    }

    let (dir, img) = fixture("success", 64, "48M");
    let t = target(&img, "1");
    let (code, _, err) = run(&["mkfs", &t, "ext4", "--yes"]);
    assert_eq!(code, 0, "the fixture must get a real ext4: {err}");

    let bytes = fs::read(&img).unwrap();
    let entries = listing(dir.path());

    let (code, out, err) = run(&["check", &t]);
    assert_eq!(code, 0, "a clean ext4 passes the check: {err}");
    assert!(out.contains("check done on partition #1 (ext)"), "{out}");

    assert_eq!(fs::read(&img).unwrap(), bytes, "a successful check must not write the target");
    assert_eq!(listing(dir.path()), entries, "a successful check must not touch the sidecars");
}

/// `TOOL_PATH` 的默认目录表（`src/fsops/env.rs`）。可执行位判据与 `find_tool` 相同：
/// 文件存在且任一 x 位置位
fn in_system_tool_path(name: &str) -> bool {
    ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"]
        .iter()
        .any(|dir| {
            let path = Path::new(dir).join(name);
            path.is_file() && path.metadata().is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        })
}
