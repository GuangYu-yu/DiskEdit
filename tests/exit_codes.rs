//! 退出码契约的进程级验收：同一张表（0–6）在命令行上的可观察落点。
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

/// 0 = 后置条件全部成立。`info` 是只读路径，读回成功即是它自己的完整成功。
/// 非 Linux 没有只读端口，这条路径在那里不可观察
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

/// 2 = argv 结构或值域不成立：未知命令、缺位置参数、未声明旗标、非法值域
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
        vec!["records", "verify", "nothex", img_s],
        vec!["records", "scan"],
    ] {
        let (code, _out, err) = run(&args);
        assert_eq!(code, 2, "`{args:?}` must be a usage error: {err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 4 = 操作失败且确认没有产生持久化修改。打开失败发生在任何写入之前，
/// 因此文案必须明确说目标未被修改（这正是 4 与 5 的分界）
#[cfg(target_os = "linux")]
#[test]
fn failure_before_any_write_is_four() {
    let dir = temp_dir("nomutation");
    let missing = dir.join("missing.img");
    let (code, _out, err) = run(&["info", missing.to_str().unwrap()]);
    assert_eq!(code, 4, "{err}");
    assert!(err.contains("no durable change"), "4 must state the target was not modified: {err}");
    assert!(!err.contains("may have changed"), "4 must not borrow the 5 wording: {err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 3 = 请求与目标现状不匹配、确定未写盘（改参数有意义）。
/// 这条路径要写命令能过平台门
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

/// 3 也覆盖“盘上的表不合规范”：结构损坏不是 I/O 失败，只读命令同样不给 4。
/// 两份条目数组都损坏时 `info` 走失败信封，退出码与写命令同一判据
#[cfg(target_os = "linux")]
#[test]
fn corrupt_table_is_three() {
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

/// 6 = 平台不具备命令所需的能力：非 Linux 镜像上纯只读命令可用，写命令因缺原语退 6。
/// 能力门是唯一判据，先于确认门与任何伴随文件创建
#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_platform_is_six() {
    let dir = temp_dir("unsupported");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    let img_s = img.to_str().unwrap();
    // 能力门是唯一判据：纯只读命令在非 Linux 镜像上放行，
    // 写命令因 WriteTable 等非 Linux 不支持而落 6，文案点名缺失原语
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
