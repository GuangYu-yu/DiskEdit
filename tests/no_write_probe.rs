#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程 / 设备）
//! 只读命令零写。
//!
//! 只读命令的 `requires` 不含写原语；不建临时文件、不 rename、不 fsync、
//! 不删除。`plan` 不打开证据路径、不触发写目录探测。
//!
//! 判定方式是黑盒的：对目标所在目录做递归快照（路径、类型、长度、内容 sha256），
//! 命令前后逐项相同才算通过。这样临时文件、改名、删除、删除后重建都能被抓到。
//!
//! 只用镜像文件，不需要 root 或外部工具；块设备侧的证据目录另由 Linux 平台用例覆盖。

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
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
    let dir = std::env::temp_dir().join(format!("diskedit-nowrite-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn image_with(dir: &Path, name: &str, size: u64) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, vec![0u8; size as usize]).expect("create image");
    path
}

/// 一条目录项：类型、字节数、内容摘要。符号链接单独成类，不跟随。
type Entry = (&'static str, u64, String);

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for path in entries {
            let rel = path.strip_prefix(root).expect("entry under root").to_path_buf();
            let meta = std::fs::symlink_metadata(&path).expect("stat entry");
            let kind = meta.file_type();
            if kind.is_dir() {
                out.insert(rel, ("dir", 0, String::new()));
                walk(root, &path, out);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&path).expect("read link");
                out.insert(rel, ("symlink", 0, target.to_string_lossy().into_owned()));
            } else {
                let bytes = std::fs::read(&path).expect("read file");
                out.insert(rel, ("file", bytes.len() as u64, sha256_hex(&bytes)));
            }
        }
    }

    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[test]
fn info_and_plan_never_create_the_sidecar() {
    let dir = temp_dir("bare");
    let img = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let p = img.to_str().unwrap();
    let before = snapshot(&dir);

    for args in [vec!["info", p], vec!["plan", p]] {
        let (code, _, err) = run(&args);
        assert_eq!(code, 0, "`{}` must be read-only success: {err}", args.join(" "));
        assert!(
            !dir.join("disk.img.diskedit").exists(),
            "`{}` probed the write directory",
            args.join(" ")
        );
        assert_eq!(
            snapshot(&dir),
            before,
            "`{}` changed the target's directory",
            args.join(" ")
        );
    }
}

#[test]
fn read_only_commands_leave_an_existing_record_area_untouched() {
    let dir = temp_dir("existing");
    let img = image_with(&dir, "disk.img", 16 * 1024 * 1024);
    let p = img.to_str().unwrap();
    let (code, _, err) = run(&["new", p, "--table", "gpt", "--yes"]);
    assert_eq!(code, 0, "setup `new` failed: {err}");
    let (code, _, err) = run(&["add", p, "1M", "--yes"]);
    assert_eq!(code, 0, "setup `add` failed: {err}");

    let before = snapshot(&dir);
    let scan_root = dir.to_str().unwrap();
    let grown = format!("{p}:1");
    for args in [
        vec!["info", p],
        vec!["plan", p],
        vec!["plan", &grown, "--grow"],
        vec!["inspect", p],
        vec!["records", "list", p],
        vec!["records", "scan", scan_root],
    ] {
        let (code, _, err) = run(&args);
        assert_eq!(code, 0, "`{}` must be read-only success: {err}", args.join(" "));
        assert_eq!(
            snapshot(&dir),
            before,
            "`{}` changed the target's directory",
            args.join(" ")
        );
    }
}
