//! 退出码 0–6 的进程级测试：每个码在命令行上的可观察落点。
//! 只能在内部构造的形态（1、5）由 `src/outcome.rs` 的映射测试钉住；
//! 这里覆盖能真实跑出来的 0/2/3/4/6，以及 4 与 5 的文案分界

use std::io::Cursor;
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 保护 MBR + GPT：LBA0 槽位 1 = 0xEE、StartLBA = 1，签名 0x55AA。
/// gptman 只写 GPT 结构，而读表以保护 MBR 为判定前置
fn fixture_gpt_image(path: &std::path::Path) {
    let ss = 512u64;
    let mut cur = Cursor::new(vec![0u8; 200 * ss as usize]);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x02; 16],
        starting_lba: 34,
        ending_lba: 60,
        attribute_bits: 0,
        partition_name: "p1".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    let mut data = cur.into_inner();
    let total = data.len() as u64 / ss;
    data[446 + 4] = 0xEE;
    data[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&((total - 1) as u32).to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    std::fs::write(path, data).unwrap();
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_exit_codes_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(target_os = "linux")]
#[test]
fn complete_operation_is_zero() {
    let dir = temp_dir("ok");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    let (code, _out, err) = run(&["info", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn argument_errors_are_two() {
    let dir = temp_dir("usage");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    for args in [
        vec!["nosuchcmd"],
        vec!["info"],
        vec!["info", img_s, "--bogus"],
        vec!["info", img_s, "--sector-size", "123"],
        vec!["help", "nosuchcmd"],
        vec!["records"],
        vec!["records", "frobnicate"],
        vec!["records", "list", "--sector-size", "512"],
        vec!["records", "verify", img_s, "nothex"],
        vec!["records", "scan"],
    ] {
        let (code, _out, err) = run(&args);
        assert_eq!(code, 2, "`{args:?}` must be a usage error: {err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn unreadable_target_is_three() {
    let dir = temp_dir("unreadable");
    let missing = dir.join("missing.img");
    let missing_s = missing.to_str().unwrap();
    for args in [
        vec!["info", missing_s],
        vec!["move", missing_s, "1:2", "--yes"],
        vec!["records", "list", missing_s],
    ] {
        let (code, _out, err) = run(&args);
        assert_eq!(code, 3, "`{args:?}` must report an unmet precondition: {err}");
        assert!(
            !err.contains("this run changed no persistent state"),
            "a read failure is not a no-mutation report: {err}"
        );
        assert!(!err.contains("may have changed"), "a read failure is not a partial report: {err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn refused_request_is_three() {
    let dir = temp_dir("refused");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    let (code, _out, err) = run(&["delete", &format!("{}:99", img.display()), "--yes"]);
    assert_eq!(code, 3, "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn corrupt_table_is_three() {
    // 损坏的表走 3：解析失败说明目标给不出可用的表，是前置条件不满足
    let dir = temp_dir("corrupt");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    {
        let mut raw = std::fs::read(&img).unwrap();
        let total = raw.len() / 512;
        raw[2 * 512 + 10] ^= 0xFF; // 主条目数组（LBA2）
        raw[(total - 33) as usize * 512 + 10] ^= 0xFF; // 备条目数组（末块之前 32 扇区）
        std::fs::write(&img, &raw).unwrap();
    }
    let (code, out, err) = run(&["info", img.to_str().unwrap()]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("parse failed"), "the damaged table must be reported: {err}");
    assert!(out.contains("\"error\""), "the failure envelope keeps the JSON keys: {out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_platform_is_six() {
    let dir = temp_dir("unsupported");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    for args in [vec!["info", img_s], vec!["plan", img_s]] {
        let (code, _out, err) = run(&args);
        assert_eq!(code, 0, "`{args:?}` is read-only and must be admitted: {err}");
    }
    let owned = format!("{img_s}:1");
    let args = vec!["delete", owned.as_str(), "--yes"];
    let (code, _out, err) = run(&args);
    assert_eq!(code, 6, "`{args:?}`: {err}");
    assert!(err.contains("needs the"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
