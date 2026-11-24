//! 单一 runner：按 `cases` 的数据建镜像、跑生产二进制、核对退出码、JSON 字段与字节摘要。

mod cases;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use cases::{Arrange, Case, Requirement};

#[test]
fn case_table_runs_from_the_production_entry() {
    let mut ran = 0_usize;
    let mut not_applicable = 0_usize;
    for case in cases::CASES {
        if let Requirement::Fixture(why) = case.requirement {
            eprintln!("NotApplicable: {} —— 需要夹具：{why}", case.name);
            not_applicable += 1;
            continue;
        }
        run_case(case);
        ran += 1;
    }
    assert!(ran >= 14, "至少应有 14 条普通文件用例实际执行，实际 {ran}");
    eprintln!("case runner: {ran} 条执行，{not_applicable} 条 NotApplicable");
}

fn run_case(case: &Case) {
    let dir = temp_dir(case.name);
    let image = dir.join("target.img");
    if case.image_bytes > 0 {
        File::create(&image)
            .and_then(|file| file.set_len(case.image_bytes))
            .unwrap_or_else(|error| panic!("{}: 创建镜像失败：{error}", case.name));
    }
    arrange(case, &image, &dir);

    let before = digest(&image);
    let argv: Vec<String> = case
        .argv
        .iter()
        .map(|token| substitute(token, &image))
        .collect();
    let output = Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(&argv)
        .output()
        .unwrap_or_else(|error| panic!("{}: 二进制无法启动：{error}", case.name));
    let code = output.status.code();
    assert_eq!(
        code,
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
            before,
            digest(&image),
            "{}: 拒绝路径改动了目标字节",
            case.name
        );
    }
    fs::remove_dir_all(&dir).ok();
}

fn arrange(case: &Case, image: &Path, _dir: &Path) {
    let target = image.to_str().expect("临时路径应为 UTF-8");
    match case.arrange {
        Arrange::Fresh => {}
        Arrange::Gpt => run_ok(case, &["new", target, "--yes"]),
        Arrange::GptWithPartition => {
            run_ok(case, &["new", target, "--yes"]);
            run_ok(case, &["add", target, "16M", "--yes"]);
        }
    }
}

fn run_ok(case: &Case, argv: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_diskedit"))
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
