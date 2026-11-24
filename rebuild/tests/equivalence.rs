//! 等价对照：同一镜像、同一命令，两个二进制逐项比对。
//!
//! 基线二进制由 `DISKEDIT_BASELINE` 指定；未提供时回报 `NotApplicable`，不静默跳过。
//! 按 D9，只读命令的人读文本与 JSON 字段集不是契约，这里不逐字比对输出；比对的是
//! 退出码、两枚镜像的结构投影（分区起止、区间长度、内容判定），以及 D8 的类型
//! GUID 不被几何变化改写。

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// 一个对照场景：前置命令与待比对的命令都以占位符写。
struct Scenario {
    name: &'static str,
    arrange: &'static [&'static [&'static str]],
    argv: &'static [&'static str],
    /// 待比对命令是否输出 `info` 形状的 JSON，需要附加顶层事实对照。
    info_shape: bool,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "inspect after new and add",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["inspect", "{image}"],
        info_shape: false,
    },
    Scenario {
        name: "info after new and add",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["info", "{image}"],
        info_shape: true,
    },
    Scenario {
        name: "new on an empty image",
        arrange: &[],
        argv: &["new", "{image}", "--yes"],
        info_shape: false,
    },
    Scenario {
        name: "add a partition",
        arrange: &[&["new", "{image}", "--yes"]],
        argv: &["add", "{image}", "16M", "--yes"],
        info_shape: false,
    },
    Scenario {
        name: "delete a partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["delete", "{image}:1", "--yes"],
        info_shape: false,
    },
    Scenario {
        name: "D8: the partition type GUID survives a resize",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
        ],
        argv: &["resize", "{image}:1", "32M", "--no-fs", "--yes"],
        info_shape: false,
    },
    Scenario {
        name: "resize grows the filesystem with the partition",
        arrange: &[
            &["new", "{image}", "--yes"],
            &["add", "{image}", "16M", "--yes"],
            &["mkfs", "{image}:1", "ext4", "--yes"],
        ],
        argv: &["resize", "{image}:1", "32M", "--yes"],
        info_shape: false,
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
    let new_binary = Path::new(env!("CARGO_BIN_EXE_diskedit"));

    let mut compared = 0;
    for scenario in SCENARIOS {
        compare(scenario, &baseline, new_binary);
        compared += 1;
    }
    assert_eq!(compared, SCENARIOS.len());
}

fn compare(scenario: &Scenario, baseline: &Path, new_binary: &Path) {
    let dir = temp_dir(scenario.name);
    let old_image = dir.join("old.img");
    let new_image = dir.join("new.img");
    for image in [&old_image, &new_image] {
        File::create(image)
            .and_then(|file| file.set_len(64 * 1024 * 1024))
            .unwrap_or_else(|error| panic!("{}: 创建镜像失败：{error}", scenario.name));
    }
    let state_dir = dir.join("state");
    let binaries: [(&Path, &PathBuf); 2] = [(baseline, &old_image), (new_binary, &new_image)];
    for (binary, image) in binaries {
        for argv in scenario.arrange {
            let output = run(binary, argv, image, &state_dir);
            assert!(
                output.status.success(),
                "{}: 前置命令失败（{}）\nstderr: {}",
                scenario.name,
                binary.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    let old = run(baseline, scenario.argv, &old_image, &state_dir);
    let new = run(new_binary, scenario.argv, &new_image, &state_dir);
    assert_eq!(
        old.status.code(),
        new.status.code(),
        "{}: 退出码不同（基线 stderr: {}）",
        scenario.name,
        String::from_utf8_lossy(&old.stderr)
    );

    assert_eq!(
        projection(new_binary, &old_image, &state_dir),
        projection(new_binary, &new_image, &state_dir),
        "{}: 两枚镜像的结构投影不同",
        scenario.name
    );

    if scenario.name.starts_with("D8") {
        assert_eq!(
            gpt_type_guid(&old_image),
            gpt_type_guid(&new_image),
            "{}: 分区类型 GUID 被改写",
            scenario.name
        );
    }

    if scenario.info_shape {
        assert_eq!(
            info_facts(&old.stdout),
            info_facts(&new.stdout),
            "{}: info 顶层事实不同",
            scenario.name
        );
    }
    fs::remove_dir_all(&dir).ok();
}

fn run(binary: &Path, argv: &[&str], image: &Path, state_dir: &Path) -> Output {
    let image = image.to_str().expect("临时路径应为 UTF-8");
    Command::new(binary)
        .env("DISKEDIT_STATE_DIR", state_dir)
        .args(argv.iter().map(|token| token.replace("{image}", image)))
        .output()
        .unwrap_or_else(|error| panic!("{} 无法启动：{error}", binary.display()))
}

/// 两枚镜像的结构投影：用本实现的 `inspect` 读取，只保留契约事实，不比对路径与
/// 未知原因的措辞。
fn projection(binary: &Path, image: &Path, state_dir: &Path) -> Value {
    let output = run(binary, &["inspect", "{image}"], image, state_dir);
    assert!(
        output.status.success(),
        "inspect 失败（{}）：{}",
        image.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("inspect 输出应为 JSON");
    let partitions: Vec<Value> = document["partitions"]
        .as_array()
        .expect("inspect 应有 partitions 数组")
        .iter()
        .map(|partition| {
            serde_json::json!({
                "number": partition["number"],
                "start_lba": partition["start_lba"],
                "sectors": partition["sectors"],
                "content": partition["content"]["state"],
            })
        })
        .collect();
    Value::Array(partitions)
}

/// `info` 的顶层事实：表类型、扇区、字节数。基线键名 `label`/`size_bytes`，
/// 本实现键名 `table`/`bytes.value`。
fn info_facts(stdout: &[u8]) -> (String, u64, u64) {
    let document: Value = serde_json::from_slice(stdout).expect("info 输出应为 JSON");
    let table = document["label"]
        .as_str()
        .or_else(|| document["table"].as_str())
        .expect("info 应给出表类型")
        .to_owned();
    let sector = document["sector_size"]
        .as_u64()
        .or_else(|| document["sector_size"]["value"].as_u64())
        .expect("info 应给出扇区大小");
    let bytes = document["size_bytes"]
        .as_u64()
        .or_else(|| document["bytes"]["value"].as_u64())
        .expect("info 应给出目标字节数");
    (table, sector, bytes)
}

/// GPT 分区 1 的类型 GUID 原始字节；没有表或没有条目时为 `None`。
fn gpt_type_guid(image: &Path) -> Option<[u8; 16]> {
    let bytes = fs::read(image).ok()?;
    let sector = 512usize;
    let header = bytes.get(sector..sector * 2)?;
    if &header[..8] != b"EFI PART" {
        return None;
    }
    let entry_lba = u64::from_le_bytes(header[72..80].try_into().ok()?) as usize;
    let entry = bytes.get(entry_lba * sector..entry_lba * sector + 16)?;
    entry.try_into().ok()
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
