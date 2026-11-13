//! 第三方并发写的可观测后果。
//!
//! 目标锁是 `flock`：外部进程不参与时写入照常落盘；参与时 diskedit 的写命令落 4，
//! 读命令不受影响。外部已写入的字节由下一次读取如实反映，表区之外的字节不被写命令波及。
//!
//! 需要 Linux：`flock` 与记录后端只在 Linux 编译。

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_concurrent_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn image_with(dir: &Path, size: u64) -> PathBuf {
    let image = dir.join("disk.img");
    fs::write(&image, vec![0u8; size as usize]).expect("create image");
    image
}

fn new_gpt(image: &Path) {
    let (code, _out, err) = run(&["new", image.to_str().expect("utf-8 path"), "--yes"]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn an_external_rewrite_is_visible_to_the_next_read() {
    let dir = temp_dir("rewrite");
    let image = image_with(&dir, 16 * 1024 * 1024);
    new_gpt(&image);

    {
        let mut file = fs::OpenOptions::new().write(true).open(&image).expect("open for external write");
        file.seek(SeekFrom::Start(0)).expect("seek");
        file.write_all(&vec![0u8; 1024 * 1024]).expect("external rewrite");
    }

    let (code, out, err) = run(&["info", image.to_str().expect("utf-8 path")]);
    assert_eq!(code, 0, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("info JSON");
    assert_eq!(json["damaged"], true, "the external rewrite must surface as damage: {out}");
    assert_eq!(json["partitions"].as_array().expect("partitions").len(), 0, "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_external_truncation_is_visible_to_the_next_read() {
    let dir = temp_dir("truncate");
    let image = image_with(&dir, 16 * 1024 * 1024);
    new_gpt(&image);
    fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .expect("open for truncation")
        .set_len(4 * 1024 * 1024)
        .expect("truncate");

    let (code, _out, err) = run(&["info", image.to_str().expect("utf-8 path")]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("truncated image"), "{err}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_external_lock_blocks_writers_but_not_readers() {
    let dir = temp_dir("lock");
    let image = image_with(&dir, 16 * 1024 * 1024);
    new_gpt(&image);
    let path = image.to_str().expect("utf-8 path");
    let before = fs::read(&image).expect("read image");

    let guard = fs::File::open(&image).expect("open image for lock");
    assert!(guard.try_lock().is_ok(), "the test must be able to hold the target lock");

    let (code, out, err) = run(&["add", path, "1M", "--yes"]);
    assert_eq!(code, 4, "{out}\n{err}");
    assert!(err.contains("another diskedit is working on this target"), "{err}");
    assert_eq!(fs::read(&image).expect("read image"), before, "a busy target must not be written");

    let (code, _out, err) = run(&["info", path]);
    assert_eq!(code, 0, "reads must not take the target lock: {err}");

    drop(guard);
    let (code, _out, err) = run(&["add", path, "1M", "--yes"]);
    assert_eq!(code, 0, "the lock must be released with its holder: {err}");
    let (code, out, err) = run(&["info", path]);
    assert_eq!(code, 0, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("info JSON");
    assert_eq!(json["partitions"].as_array().expect("partitions").len(), 1, "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn bytes_outside_the_table_survive_a_table_change() {
    let dir = temp_dir("scope");
    let image = image_with(&dir, 16 * 1024 * 1024);
    new_gpt(&image);
    let offset = 2 * 1024 * 1024;
    {
        let mut file = fs::OpenOptions::new().write(true).open(&image).expect("open for marker");
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(b"MARKER").expect("write marker");
    }

    let (code, _out, err) = run(&["add", image.to_str().expect("utf-8 path"), "1M", "--yes"]);
    assert_eq!(code, 0, "{err}");

    let bytes = fs::read(&image).expect("read image");
    assert_eq!(&bytes[offset as usize..offset as usize + 6], b"MARKER");
    fs::remove_dir_all(&dir).ok();
}
