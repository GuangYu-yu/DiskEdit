//! 平台 × 命令的出口矩阵（镜像形态）。
//!
//! 块设备形态由 `tests/wsl/` 下的回归用例以 loop 设备取证；
//! 这里钉住能力门：Linux 放行全部命令；非 Linux 只有镜像可打开，纯只读命令
//! （info/plan）放行，其余按缺失原语退 6。帮助是元命令，由参数层直接退出，
//! 不经过这道门（见 `tests/cli/platform_gate.rs`）。

#![allow(clippy::let_underscore_must_use)] // 临时目录清理有意忽略失败

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(not(target_os = "linux"))]
const CAPABILITY_GAP: &str = "needs the";

/// 16 MiB GPT 镜像，分区 1 = 2048..4095
fn write_gpt(path: &Path) {
    let ss = 512u64;
    let mut cur = Cursor::new(vec![0u8; 32768 * ss as usize]);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x02; 16],
        starting_lba: 2048,
        ending_lba: 4095,
        attribute_bits: 0,
        partition_name: "p1".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(path, with_protective_mbr(cur.into_inner())).unwrap();
}

/// 补保护 MBR：gptman 只写 GPT 结构，而读取路径以保护 MBR 为判定前置
fn with_protective_mbr(mut data: Vec<u8>) -> Vec<u8> {
    let total = data.len() as u64 / 512;
    let size: u32 = if total > u32::MAX as u64 { u32::MAX } else { (total - 1) as u32 };
    data[446 + 4] = 0xEE;
    data[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&size.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    data
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 每条用例独立目录：前一条被写坏的表不会污染后一条的判据
fn case(name: &str) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("diskedit_matrix_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    write_gpt(&img);
    let img_s = img.to_string_lossy().into_owned();
    (dir, img_s)
}

#[cfg(target_os = "linux")]
#[test]
fn read_only_commands_are_available_on_linux() {
    let (dir, img) = case("ro");
    let (code, _out, err) = run(&["info", &img]);
    assert_eq!(code, 0, "`info` is read-only and stays available: {err}");
    let (code, _out, err) = run(&["plan", &img]);
    assert_eq!(code, 0, "`plan` is read-only and stays available: {err}");
    let (code, _out, err) = run(&["abandon", &img]);
    assert_eq!(code, 0, "`abandon` only touches companion files: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn inspect_of_an_image_without_a_record_succeeds() {
    let (dir, img) = case("inspect");
    // `record: absent` 的前提是记录落点存在：`inspect` 先打开 `<img>.diskedit`，
    // 落点缺席按 `inspect --help` 的「anchor directory cannot be read」落 4
    std::fs::create_dir_all(format!("{img}.diskedit")).unwrap();
    let (code, out, err) = run(&["inspect", &img]);
    assert_eq!(code, 0, "an absent record is not a failure: {err}");
    assert!(out.contains("record: absent"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 非 Linux × 镜像：纯只读的 `info` / `plan` 可用，
/// 它们不打开证据后端、不写盘，也不承诺恢复
#[cfg(not(target_os = "linux"))]
#[test]
fn read_only_commands_run_on_images() {
    for name in ["info", "plan"] {
        let (dir, img) = case(name);
        let before = std::fs::read(&img).unwrap();
        let (code, out, err) = run(&[name, &img]);
        assert_eq!(code, 0, "`{name}` is read-only and must be admitted: {err}");
        assert!(!out.is_empty(), "`{name}` must report the layout: {err}");
        assert_eq!(std::fs::read(&img).unwrap(), before, "`{name}` must not modify the image");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 非 Linux：任何需要写侧或证据侧原语的命令都停在能力门，文案点名缺失原语
#[cfg(not(target_os = "linux"))]
#[test]
fn write_and_evidence_commands_stop_at_the_capability_gate() {
    let cases: &[&str] = &[
        "inspect", "resize", "move", "copy", "delete", "set", "check", "mkfs",
        "resizefs", "undo", "abandon", "new", "add", "resize-part",
    ];
    for name in cases {
        let (dir, img) = case(name);
        let p = format!("{img}:1");
        let full: Vec<&str> = match *name {
            "inspect" => vec![name, &img],
            "resize" => vec![name, &p, "+1M", "--no-fs", "--yes"],
            "move" => vec![name, &p, "--start", "4096", "--chunk", "4", "--yes"],
            "copy" => vec![name, &p, "--start", "8192", "--chunk", "4", "--yes"],
            "delete" => vec![name, &p, "--yes"],
            "set" => vec![name, &p, "name", "renamed", "--yes"],
            "check" => vec![name, &p],
            "resizefs" => vec![name, &p, "--yes"],
            "mkfs" => vec![name, &p, "ext4", "--yes"],
            "undo" => vec![name, &img, "--yes"],
            "abandon" => vec![name, &img],
            "new" => vec![name, &img, "--yes"],
            "add" => vec![name, &img, "1M", "--yes"],
            "resize-part" => vec![name, &p, "--end", "8191", "--yes"],
            _ => unreachable!(),
        };
        let (code, _out, err) = run(&full);
        assert_eq!(code, 6, "`{name}` must stop at the capability gate: {err}");
        assert!(err.contains(CAPABILITY_GAP), "`{name}`: {err}");
        assert!(err.contains("Nothing was written"), "`{name}`: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn records_commands_stop_at_the_capability_gate() {
    let (dir, _img) = case("records");
    let root = dir.to_str().unwrap().to_string();
    let id = "00112233445566778899aabbccddeeff";
    let cases: [&[&str]; 4] = [
        &["records", "list"],
        &["records", "list", &root],
        &["records", "verify", id, &root],
        &["records", "scan", &root],
    ];
    for args in cases {
        let (code, _out, err) = run(args);
        assert_eq!(code, 6, "`{args:?}` must stop at the capability gate: {err}");
        assert!(err.contains(CAPABILITY_GAP), "`{args:?}`: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn write_commands_run_on_linux() {
    // new 的目标是空镜像：预写的表会撞覆盖门（已有表 → 3），本段验的是平台放行
    let dir = std::env::temp_dir().join(format!("diskedit_matrix_new_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let img_s = img.to_string_lossy().into_owned();
    let (code, _out, err) = run(&["new", &img_s, "--yes"]);
    assert_eq!(code, 0, "`new` is admitted here: {err}");
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, img) = case("add");
    let (code, _out, err) = run(&[
        "add",
        &img,
        "512K",
        "--type-guid",
        "0fc63daf-8483-4772-8e79-3d69d8477de4",
        "--yes",
    ]);
    assert_eq!(code, 0, "`add` is admitted here: {err}");
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, img) = case("delete");
    let p = format!("{img}:1");
    let (code, _out, err) = run(&["delete", &p, "--yes"]);
    assert_eq!(code, 0, "`delete` is admitted here: {err}");
    let _ = std::fs::remove_dir_all(&dir);

    // 没有历史可撤销时 `undo` 退 3：它先过了能力门，缺的是凭据而不是平台保证
    let (dir, img) = case("undo");
    let (code, _out, err) = run(&["undo", &img, "--yes"]);
    assert_eq!(code, 3, "`undo` without history is a refusal, not a gate stop: {err}");
    assert!(err.contains("nothing to undo"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
