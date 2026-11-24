//! 单一 runner：按 `cases` 的数据建镜像、跑生产二进制、核对退出码、JSON 字段与字节摘要。
//!
//! 每条用例独占一个状态目录（`DISKEDIT_STATE_DIR`），因此记录、锁与归档不会在
//! 用例之间串味。需要现场能力的用例由 runner 现搭现拆：loop 设备用 `losetup`
//! 挂上再释放，外来记录按目标身份预置到同一状态目录。

mod cases;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cases::{Arrange, Case, Requirement};

#[test]
fn case_table_runs_from_the_production_entry() {
    let mut ran = 0_usize;
    let mut not_applicable = 0_usize;
    for case in cases::CASES {
        match run_case(case) {
            Ok(()) => ran += 1,
            Err(why) => {
                eprintln!("NotApplicable: {} —— {why}", case.name);
                not_applicable += 1;
            }
        }
    }
    eprintln!("case runner: {ran} 条执行，{not_applicable} 条 NotApplicable");
    assert_eq!(
        ran + not_applicable,
        cases::CASES.len(),
        "每条用例都必须被判定为执行或明确 NotApplicable"
    );
    assert!(ran >= 16, "至少应有 16 条用例实际执行，实际 {ran}");
}

/// 跑一条用例；返回 `Err` 表示环境不提供所需现场。
fn run_case(case: &Case) -> Result<(), String> {
    let dir = temp_dir(case.name);
    let image = dir.join("target.img");
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("创建状态目录失败");
    if case.image_bytes > 0 {
        File::create(&image)
            .and_then(|file| file.set_len(case.image_bytes))
            .unwrap_or_else(|error| panic!("{}: 创建镜像失败：{error}", case.name));
    }
    arrange(case, &image, &state);

    if case.requirement == Requirement::ForeignRecord {
        plant_foreign_record(case, &image, &state);
    }

    let loop_device = if case.requirement == Requirement::LoopBacked {
        Some(loop_attach(&image).ok_or_else(|| "缺少可用的 loop 设备".to_owned())?)
    } else {
        None
    };

    let before = digest(&image);
    let argv: Vec<String> = case
        .argv
        .iter()
        .map(|token| substitute(token, &image))
        .collect();
    let output = command(&state)
        .args(&argv)
        .output()
        .unwrap_or_else(|error| panic!("{}: 二进制无法启动：{error}", case.name));
    if let Some(device) = &loop_device {
        Command::new("losetup").arg("-d").arg(device).status().ok();
    }
    check(case, &image, &before, &output);
    fs::remove_dir_all(&dir).ok();
    Ok(())
}

fn check(case: &Case, image: &Path, before: &Option<u64>, output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(case.expected_code),
        "{}: 退出码不符\nstdout: {}\nstderr: {}",
        case.name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    for fragment in case.expected_stdout {
        assert!(
            combined.contains(fragment),
            "{}: 输出缺少 `{fragment}`\nstdout: {stdout}\nstderr: {stderr}",
            case.name
        );
    }
    if !case.expected_json_keys.is_empty() {
        let document: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|error| panic!("{}: stdout 不是 JSON：{error}\n{stdout}", case.name));
        let object = document
            .as_object()
            .unwrap_or_else(|| panic!("{}: stdout 不是 JSON 对象", case.name));
        for key in case.expected_json_keys {
            assert!(
                object.contains_key(*key),
                "{}: JSON 缺少键 `{key}`",
                case.name
            );
        }
    }
    if case.unchanged {
        assert_eq!(
            *before,
            digest(image),
            "{}: 拒绝路径改动了目标字节",
            case.name
        );
    }
}

fn arrange(case: &Case, image: &Path, state: &Path) {
    let target = image.to_str().expect("临时路径应为 UTF-8");
    match case.arrange {
        Arrange::Fresh => {}
        Arrange::Gpt => run_ok(case, state, &["new", target, "--yes"]),
        Arrange::GptWithPartition => {
            run_ok(case, state, &["new", target, "--yes"]);
            run_ok(case, state, &["add", target, "16M", "--yes"]);
        }
    }
}

fn run_ok(case: &Case, state: &Path, argv: &[&str]) {
    let output = command(state)
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("{}: 前置命令无法启动：{error}", case.name));
    assert!(
        output.status.success(),
        "{}: 前置命令 {argv:?} 失败\nstdout: {}\nstderr: {}",
        case.name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 按目标身份找到记录目录，写入一份外来实现的字节。
fn plant_foreign_record(case: &Case, image: &Path, state: &Path) {
    let target = image.to_str().expect("临时路径应为 UTF-8");
    let output = command(state)
        .args(["info", target])
        .output()
        .unwrap_or_else(|error| panic!("{}: 读取身份失败：{error}", case.name));
    assert!(
        output.status.success(),
        "{}: info 失败\nstdout: {}",
        case.name,
        String::from_utf8_lossy(&output.stdout)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("info 输出应为 JSON");
    let identity = document["identity"]
        .as_str()
        .unwrap_or_else(|| panic!("{}: info 缺少 identity", case.name));
    let key = &identity[..32];
    let records = state.join("recovery").join(key);
    fs::create_dir_all(&records).expect("创建记录目录失败");
    fs::write(records.join("record.bin"), b"foreign-implementation-record")
        .expect("写入外来记录失败");
}

/// 把一个镜像挂到空闲 loop 设备上；返回设备路径。
fn loop_attach(image: &Path) -> Option<String> {
    let output = Command::new("losetup")
        .arg("-f")
        .arg("--show")
        .arg(image)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let device = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!device.is_empty()).then_some(device)
}

fn command(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_diskedit"));
    command.env("DISKEDIT_STATE_DIR", state);
    command
}

fn substitute(token: &str, image: &Path) -> String {
    token.replace("{image}", image.to_str().expect("临时路径应为 UTF-8"))
}

fn temp_dir(name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let dir = std::env::temp_dir().join(format!("diskedit-case-{}-{slug}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir
}

/// FNV-1a 64 位摘要，用来比较运行前后的目标字节。
fn digest(path: &Path) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(hash)
}
