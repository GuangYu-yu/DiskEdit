//! 等价对照：同一镜像、同一命令，两个二进制逐项比对。
//!
//! 基线二进制由 `DISKEDIT_BASELINE` 指定；未提供时回报 `NotApplicable`，不静默跳过。
//! 比对退出码、stdout、stderr、JSON 文档与运行后的目标字节摘要。

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// 一个对照场景：前置命令与待比对的命令都以占位符写。
struct Scenario {
    name: &'static str,
    arrange: &'static [&'static [&'static str]],
    argv: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "inspect after new and add",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["inspect", "{image}"],
    },
    Scenario {
        name: "info after new and add",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["info", "{image}"],
    },
    Scenario {
        name: "new on an empty image",
        arrange: &[],
        argv: &["new", "{image}", "--yes"],
    },
    Scenario {
        name: "add a partition",
        arrange: &[&["new", "{image}", "--yes"]],
        argv: &["add", "{image}", "16M", "--yes"],
    },
    Scenario {
        name: "delete a partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["delete", "{image}:1", "--yes"],
    },
    Scenario {
        name: "D8: the partition type GUID survives a resize",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["resize", "{image}:1", "32M", "--yes"],
    },
];

#[test]
fn both_binaries_agree_on_the_same_image() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，等价对照需要旧二进制");
        return;
    };
    let baseline = PathBuf::from(baseline);
    assert!(
        baseline.is_file(),
        "DISKEDIT_BASELINE 指向的文件不存在：{}",
        baseline.display()
    );

    let mut compared = 0;
    for scenario in SCENARIOS {
        compare(scenario, &baseline);
        compared += 1;
    }
    assert!(compared >= SCENARIOS.len());
}

fn compare(scenario: &Scenario, baseline: &Path) {
    let dir = temp_dir(scenario.name);
    let old_image = dir.join("old.img");
    let new_image = dir.join("new.img");
    for image in [&old_image, &new_image] {
        File::create(image)
            .and_then(|file| file.set_len(64 * 1024 * 1024))
            .unwrap_or_else(|error| panic!("{}: 创建镜像失败：{error}", scenario.name));
    }
    let binaries: [(&Path, &PathBuf); 2] = [
        (baseline, &old_image),
        (Path::new(env!("CARGO_BIN_EXE_diskedit")), &new_image),
    ];
    for (binary, image) in binaries {
        for argv in scenario.arrange {
            let output = run(binary, argv, image);
            assert!(
                output.status.success(),
                "{}: 前置命令失败（{}）\nstderr: {}",
                scenario.name,
                binary.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let old = run(baseline, scenario.argv, &old_image);
    let new = run(
        Path::new(env!("CARGO_BIN_EXE_diskedit")),
        scenario.argv,
        &new_image,
    );
    assert_eq!(
        old.status.code(),
        new.status.code(),
        "{}: 退出码不同",
        scenario.name
    );
    assert_eq!(
        normalize(&old, &old_image),
        normalize(&new, &new_image),
        "{}: stdout/stderr 不同",
        scenario.name
    );
    assert_eq!(
        digest(&old_image),
        digest(&new_image),
        "{}: 运行后的目标字节不同",
        scenario.name
    );
    fs::remove_dir_all(&dir).ok();
}

fn run(binary: &Path, argv: &[&str], image: &Path) -> Output {
    let image = image.to_str().expect("临时路径应为 UTF-8");
    Command::new(binary)
        .args(argv.iter().map(|token| token.replace("{image}", image)))
        .output()
        .unwrap_or_else(|error| panic!("{} 无法启动：{error}", binary.display()))
}

/// 去掉临时路径差异后的输出。JSON 文档按值比较，其余按文本比较。
fn normalize(output: &Output, image: &Path) -> String {
    let image = image.to_str().expect("临时路径应为 UTF-8");
    let stdout = String::from_utf8_lossy(&output.stdout).replace(image, "<image>");
    let stderr = String::from_utf8_lossy(&output.stderr).replace(image, "<image>");
    match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(value) => format!("json:{value}"),
        Err(_) => format!("text:{stdout}\n{stderr}"),
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let dir = std::env::temp_dir().join(format!("diskedit-equiv-{}-{slug}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir
}

/// FNV-1a 64 位摘要。
fn digest(path: &Path) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(hash)
}
