#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程）
//! 退出码 0–6 的进程级行为快照。
//!
//! | 码 | 含义 | 钉在哪 |
//! |---|---|---|
//! | 0 | 请求完成且没有未完成的后置条件 | 本文件 `complete_operation_is_zero`（Linux）、`unsupported_platform_is_six` 里的只读分支；`tests/cli/*` 的写命令成功用例 |
//! | 2 | 用法错误（解析期拒绝，未打开目标） | 本文件 `argument_errors_are_two` |
//! | 3 | 请求与目标现状不符，或目标打不开；本次未改变持久状态 | 本文件 `unreadable_target_is_three`、`refused_request_is_three`、`corrupt_table_is_three` |
//! | 4 | I/O 失败：目标读不出，或证据源读不出 | `outcome::tests::fs_error_maps_to_exit_codes` 钉映射；进程级见本文件 `read_failure_is_four`（`DISKEDIT_FAULT=read-at` 注入目标读失败）与 `tests/wsl/check_exit_codes.sh`（工具找得到但启动失败） |
//! | 5 | 部分完成：越过首个写点后失败，或有未完成的后置条件 | `tests/cli/*`（如 `record.rs` 的误删记录、`crash_recovery.rs` 的占用拒绝）与 `tests/wsl/*` |
//! | 6 | 能力缺口：平台不支持，或所需外部工具缺失 | 非 Linux 的 `unsupported_platform_is_six`；工具缺失见 `tests/wsl/check_exit_codes.sh` |
//!
//! I/O 归责在三处落定：打开失败 ⇒ 3，读写目标失败经 `FsError::Io` ⇒ 4，越过写点之后
//! ⇒ 5（直接用 `?` 摊平的 `io::Error` 走安全缺省 5，见 `outcome::From<io::Error>`）。
//! 码 4 与码 3 一样以 `— this run changed no persistent state` 收尾：`FsError::Io` 在
//! 映射里落 `NoMutation`，与 `Refused` 共用同一句尾注。码 3 不设例外（常量文档
//! `src/outcome.rs`、实现 `report`、`docs/capability-matrix.md:72` 三方一致）。
//!
//! 资源释放待办与结果形态的四组合由 `src/main.rs` 的单元测试 `release_leak_matrix`
//! 钉住：成功降 5 并给提示，失败保持自己的码且不给提示。

use std::io::Cursor;
use std::process::Command;

/// 本次测试进程的镜像状态根：子进程与测试算出的落点必须一致
fn state_root() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("diskedit_exit_codes_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        private_dir(&dir);
        dir
    })
}

/// 记录仓库的每一级都按 0700 建立：写命令会复核状态根、`images` 与键目录
fn private_dir(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).unwrap();
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).unwrap();
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .env("DISKEDIT_STATE_DIR", state_root())
        .output()
        .unwrap();
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
    // 命令面外的用例逐条带 `cli-surface: refused`：`tests/cli_surface_markers.rs` 的
    // 反向校验要求「落在命令面外 + 断言 2」的调用都有标记
    let (code, _out, err) = run(&["nosuchcmd"]); // cli-surface: refused
    assert_eq!(code, 2, "`nosuchcmd` must be a usage error: {err}");
    let (code, _out, err) = run(&["info"]); // 缺少目标：命令面内，参数不全
    assert_eq!(code, 2, "`info` without a target must be a usage error: {err}");
    let (code, _out, err) = run(&["info", img_s, "--bogus"]); // cli-surface: refused
    assert_eq!(code, 2, "an unknown flag must be a usage error: {err}");
    let (code, _out, err) = run(&["info", img_s, "--sector-size", "123"]);
    assert_eq!(code, 2, "an unsupported sector size must be a usage error: {err}");
    let (code, _out, err) = run(&["help", "nosuchcmd"]); // 帮助主题未知：命令面内
    assert_eq!(code, 2, "an unknown help topic must be a usage error: {err}");
    let (code, _out, err) = run(&["records"]); // cli-surface: refused
    assert_eq!(code, 2, "`records` without a subcommand must be a usage error: {err}");
    let (code, _out, err) = run(&["records", "frobnicate"]); // cli-surface: refused
    assert_eq!(code, 2, "an unknown subcommand must be a usage error: {err}");
    let (code, _out, err) = run(&["records", "list", "--sector-size", "512"]); // cli-surface: refused
    assert_eq!(code, 2, "`records list` declares no options: {err}");
    let (code, _out, err) = run(&["records", "verify", img_s, "nothex"]);
    assert_eq!(code, 2, "a non-hex record id must be a usage error: {err}");
    let (code, _out, err) = run(&["records", "scan"]);
    assert_eq!(code, 2, "`records scan` requires a root: {err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 打不开目标即拒绝（3）。只收真正打开目标的命令：`records list` 按路径定位记录目录，
/// 目标不可读时给出空列表（0），不属这一类
#[cfg(target_os = "linux")]
#[test]
fn unreadable_target_is_three() {
    let dir = temp_dir("unreadable");
    let missing = dir.join("missing.img");
    let missing_s = missing.to_str().unwrap();
    let moved = format!("{missing_s}:1");
    for args in [
        vec!["info", missing_s],
        vec!["move", moved.as_str(), "--start", "1", "--yes"],
    ] {
        let (code, _out, err) = run(&args);
        assert_eq!(code, 3, "`{args:?}` must report an unmet precondition: {err}");
        // 3 与 4 都以这句收尾：本次运行没有改变持久状态（docs/capability-matrix.md 的声明）
        assert!(
            err.contains("this run changed no persistent state"),
            "a refusal must state that this run changed no persistent state: {err}"
        );
        assert!(!err.contains("may have changed"), "a read failure is not a partial report: {err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(not(target_os = "linux"))]
#[test]
fn unreadable_target_is_three() {
    let dir = temp_dir("unreadable");
    let missing = dir.join("missing.img");
    let (code, _out, err) = run(&["info", missing.to_str().unwrap()]);
    assert_eq!(code, 3, "{err}");
    assert!(
        err.contains("this run changed no persistent state"),
        "a refusal must state that this run changed no persistent state: {err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 目标读不出是环境故障（4），且本次未改变任何持久状态。
///
/// 注入点只在 `test-faults` 构建里：`DISKEDIT_FAULT=read-at` 让 `FileSource::read_at`
/// 返回 `EIO`，读取发生在任何写入之前。命令取 `plan --grow`：只读命令在所有平台都放行，
/// 写命令在非 Linux 由能力门拒成 6，读不到这条路径
#[cfg(feature = "test-faults")]
#[test]
fn read_failure_is_four() {
    let dir = temp_dir("read_fault");
    let img = dir.join("disk.img");
    fixture_gpt_image(&img);
    let target = format!("{}:1", img.display());
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .env("DISKEDIT_FAULT", "read-at")
        .env("DISKEDIT_STATE_DIR", state_root())
        .args(["plan", &target, "--grow"])
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code, 4, "a target read failure must be an I/O failure: {err}");
    assert!(err.contains("EIO"), "the failure must name the read error: {err}");
    assert!(
        err.contains("this run changed no persistent state"),
        "a read failure must state that this run changed no persistent state: {err}"
    );
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
