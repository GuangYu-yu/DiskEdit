//! `--align` 的缺省、取值域与扇区换算。
//!
//! `Align` 域类型是 `Mib(n>=1)` 或 `None`，CLI 值域只有 `mib`（= `Mib(1)`）
//! 与 `none`；旗标字段缺省 `none`，但 `add` 省略 `--start` 且未给 `--align` 时自动
//! 放置按 1 MiB（`placement_unit`），显式 `--start` 与其它命令按给定 LBA 原样。
//! `new` 不接受该选项。显式 `--align none` 从第一个空闲 LBA 起排。
//!
//! 需要 Linux：写命令的能力门在非 Linux 上全部落 6，无法观测几何。

#![cfg(target_os = "linux")]

use std::fs;
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
    let dir = std::env::temp_dir().join(format!("diskedit_align_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn image_with(dir: &Path, name: &str, size: u64) -> PathBuf {
    let image = dir.join(name);
    fs::write(&image, vec![0u8; size as usize]).expect("create image");
    image
}

fn first_lba(info_json: &str) -> u64 {
    let json: serde_json::Value = serde_json::from_str(info_json).expect("info JSON");
    json["partitions"][0]["first_lba"].as_u64().expect("first_lba")
}

fn gpt_image(dir: &Path, name: &str, sector_size: &str) -> PathBuf {
    let image = image_with(dir, name, 32 * 1024 * 1024);
    let (code, _out, err) = run(&[
        "new",
        image.to_str().expect("utf-8 path"),
        "--table",
        "gpt",
        "--sector-size",
        sector_size,
        "--yes",
    ]);
    assert_eq!(code, 0, "{err}");
    image
}

#[test]
fn add_without_align_starts_at_the_first_mib_aligned_lba() {
    // 第 6、11 条：省略 `--align` 时，自动放置按 1 MiB 对齐（512 字节扇区下是 LBA 2048）。
    // 显式 `--start` 不在此列：起点是用户给的精确 LBA，不隐式挪动。
    let dir = temp_dir("default");
    let image = gpt_image(&dir, "disk.img", "512");
    let path = image.to_str().expect("utf-8 path");

    let (code, _out, err) = run(&["add", path, "1M", "--yes"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run(&["info", path]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(first_lba(&out), 2048, "the default placement must be 1 MiB aligned: {out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn explicit_none_uses_the_given_lbas() {
    // `None` 是显式值——自动放置从第一个空闲 LBA 起排，不隐式对齐。
    let dir = temp_dir("none");
    let default_image = gpt_image(&dir, "default.img", "512");
    let none_image = gpt_image(&dir, "none.img", "512");

    assert_eq!(run(&["add", default_image.to_str().unwrap(), "1M", "--yes"]).0, 0);
    assert_eq!(
        run(&["add", none_image.to_str().unwrap(), "1M", "--align", "none", "--yes"]).0,
        0
    );

    let (_, default_out, _) = run(&["info", default_image.to_str().unwrap()]);
    let (_, none_out, _) = run(&["info", none_image.to_str().unwrap()]);
    assert_eq!(first_lba(&default_out), 2048, "{default_out}");
    assert_eq!(first_lba(&none_out), 34, "explicit none must use the first free LBA: {none_out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn mib_alignment_is_converted_into_the_declared_sector_size() {
    // 第 6、23 条：对齐值以字节给出，落到目标扇区大小上。
    // 1 MiB / 512 = 2048，1 MiB / 4096 = 256。
    let dir = temp_dir("mib");
    for (sector_size, expected) in [("512", 2048u64), ("4096", 256u64)] {
        let image = gpt_image(&dir, &format!("disk_{sector_size}.img"), sector_size);
        let path = image.to_str().expect("utf-8 path");
        let (code, _out, err) = run(&["add", path, "1M", "--align", "mib", "--sector-size", sector_size, "--yes"]);
        assert_eq!(code, 0, "{err}");

        let (code, out, err) = run(&["info", path, "--sector-size", sector_size]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(first_lba(&out), expected, "sector size {sector_size}: {out}");
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_illegal_alignment_is_a_usage_error() {
    // 值域之外一律 2，且不改目标。
    let dir = temp_dir("illegal");
    let image = gpt_image(&dir, "disk.img", "512");
    let path = image.to_str().expect("utf-8 path");

    for value in ["0", "mib0", "cyl", "1MiB"] {
        let (code, out, err) = run(&["add", path, "1M", "--align", value, "--yes"]);
        assert_eq!(code, 2, "--align {value}: {out}\n{err}");
        assert!(err.contains("invalid alignment"), "--align {value}: {err}");
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn new_does_not_accept_align() {
    // `new` 不接受 `--align`。
    let dir = temp_dir("new");
    let image = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let (code, _out, err) = run(&[
        "new",
        image.to_str().expect("utf-8 path"),
        "--align",
        "none",
        "--yes",
    ]); // cli-surface: refused
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--align"), "{err}");
    fs::remove_dir_all(&dir).ok();
}
