//! 等价对照：同一 argv、同一现场，两个二进制比对退出码。
//!
//! 基线二进制由 `DISKEDIT_BASELINE` 指定；未提供时回报 `NotApplicable`，不静默跳过。
//! S0 只覆盖进程级出口（帮助、版本、用法）；命令行为按纵切加入对照。

use std::process::Command;

mod fixtures;

struct Scenario {
    name: &'static str,
    argv: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "top-level help",
        argv: &["--help"],
    },
    Scenario {
        name: "short help flag",
        argv: &["-h"],
    },
    Scenario {
        name: "help subcommand",
        argv: &["help"],
    },
    Scenario {
        name: "version",
        argv: &["--version"],
    },
    Scenario {
        name: "short version flag",
        argv: &["-V"],
    },
    Scenario {
        name: "no arguments",
        argv: &[],
    },
    Scenario {
        name: "unknown command",
        argv: &["bogus"],
    },
    Scenario {
        name: "help with a topic",
        argv: &["help", "layout"],
    },
    Scenario {
        name: "help with a two-word topic",
        argv: &["help", "records", "list"],
    },
    Scenario {
        name: "help flag after a command",
        argv: &["layout", "--help"],
    },
    Scenario {
        name: "help flag before a command",
        argv: &["--help", "layout"],
    },
    Scenario {
        name: "unknown help topic",
        argv: &["help", "bogus"],
    },
    Scenario {
        name: "incomplete help topic",
        argv: &["help", "records"],
    },
    Scenario {
        name: "missing target",
        argv: &["layout"],
    },
    Scenario {
        name: "unknown option",
        argv: &["layout", "--bogus"],
    },
    Scenario {
        name: "layout on a GPT image",
        argv: &["layout", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "layout on an MBR image",
        argv: &["layout", "target/fixtures/s1/mbr.img"],
    },
    Scenario {
        name: "layout on an empty image",
        argv: &["layout", "target/fixtures/s1/empty.img"],
    },
    Scenario {
        name: "layout on a damaged GPT image",
        argv: &["layout", "target/fixtures/s1/damaged.img"],
    },
    Scenario {
        name: "layout rejects a partition selector",
        argv: &["layout", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "add --dry-run on a GPT image",
        argv: &["add", "target/fixtures/s1/gpt.img", "1M", "--dry-run"],
    },
    Scenario {
        name: "add --dry-run with a slot hint",
        argv: &[
            "add",
            "target/fixtures/s1/gpt.img",
            "1M",
            "--part-number",
            "3",
            "--dry-run",
        ],
    },
    Scenario {
        name: "add --dry-run on MBR",
        argv: &["add", "target/fixtures/s1/mbr.img", "1M", "--dry-run"],
    },
    Scenario {
        name: "layout with an invalid sector size",
        argv: &[
            "layout",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "123",
        ],
    },
    Scenario {
        name: "layout with a 4096 sector size",
        argv: &[
            "layout",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "4096",
        ],
    },
    Scenario {
        name: "layout on a missing target",
        argv: &["layout", "target/fixtures/s1/missing.img"],
    },
    Scenario {
        name: "records show",
        argv: &["records", "show", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "records show on a missing target",
        argv: &["records", "show", "target/fixtures/s1/missing.img"],
    },
    Scenario {
        name: "records list without a target",
        argv: &["records", "list"],
    },
    Scenario {
        name: "records list on a missing target",
        argv: &["records", "list", "target/fixtures/s1/missing.img"],
    },
    Scenario {
        name: "records verify without a record",
        argv: &[
            "records",
            "verify",
            "target/fixtures/s1/gpt.img",
            "00000000000000000000000000000000",
        ],
    },
    Scenario {
        name: "records verify with a bad id",
        argv: &["records", "verify", "target/fixtures/s1/gpt.img", "zz"],
    },
    Scenario {
        name: "records list --root a directory",
        argv: &["records", "list", "--root", "target/fixtures/s1"],
    },
    Scenario {
        name: "records list --root a missing directory",
        argv: &["records", "list", "--root", "target/fixtures/s1/missing"],
    },
    Scenario {
        name: "check on an identified filesystem",
        argv: &["check", "target/fixtures/s1/ext.img:1"],
    },
    Scenario {
        name: "check on an unidentified partition",
        argv: &["check", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "check without a partition",
        argv: &["check", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "check on a missing target",
        argv: &["check", "target/fixtures/s1/missing.img:1"],
    },
    Scenario {
        name: "layout on a target smaller than one sector",
        argv: &["layout", "target/fixtures/s1/tiny.img"],
    },
    Scenario {
        name: "layout on an empty file",
        argv: &["layout", "target/fixtures/s1/zero.img"],
    },
    Scenario {
        name: "add --dry-run refuses a partition selector",
        argv: &["add", "target/fixtures/s1/gpt.img:1", "1M", "--dry-run"],
    },
    Scenario {
        name: "layout with a missing option value",
        argv: &["layout", "target/fixtures/s1/gpt.img", "--sector-size"],
    },
    Scenario {
        name: "layout with a repeated value option",
        argv: &[
            "layout",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "512",
            "--sector-size",
            "4096",
        ],
    },
    Scenario {
        name: "layout with a value on a bare flag",
        argv: &["layout", "--json=1", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "layout with an extra argument",
        argv: &["layout", "target/fixtures/s1/gpt.img", "extra"],
    },
    Scenario {
        name: "records show without a target",
        argv: &["records", "show"],
    },
    Scenario {
        name: "records show refuses a partition selector",
        argv: &["records", "show", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "records list refuses a partition selector",
        argv: &["records", "list", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "records list --alias without a target",
        argv: &["records", "list", "--alias", "declared-name"],
    },
    Scenario {
        name: "records list --alias on an image",
        argv: &[
            "records",
            "list",
            "--alias",
            "declared-name",
            "target/fixtures/s1/gpt.img",
        ],
    },
    Scenario {
        name: "records verify without an id",
        argv: &["records", "verify", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "records verify on an unreadable target",
        argv: &[
            "records",
            "verify",
            "target/fixtures/s1",
            "00000000000000000000000000000000",
        ],
    },
    Scenario {
        name: "records list --root without a value",
        argv: &["records", "list", "--root"],
    },
    Scenario {
        name: "records list --root refuses a file",
        argv: &["records", "list", "--root", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "check on a directory",
        argv: &["check", "target/fixtures/s1:1"],
    },
    Scenario {
        name: "new refuses a partition selector",
        argv: &["new", "target/fixtures/s2/blank.img:1", "--yes"],
    },
    Scenario {
        name: "add refuses a partition selector",
        argv: &["add", "target/fixtures/s1/gpt.img:1", "1M", "--yes"],
    },
    Scenario {
        name: "add refuses a relative size",
        argv: &["add", "target/fixtures/s1/gpt.img", "+1M", "--yes"],
    },
    Scenario {
        name: "add refuses a zero size",
        argv: &["add", "target/fixtures/s1/gpt.img", "0", "--yes"],
    },
    Scenario {
        name: "add refuses a target without a table",
        argv: &["add", "target/fixtures/s1/empty.img", "1M", "--yes"],
    },
    Scenario {
        name: "add refuses a damaged table",
        argv: &["add", "target/fixtures/s1/damaged.img", "1M", "--yes"],
    },
    Scenario {
        name: "delete refuses a target without a table",
        argv: &["delete", "target/fixtures/s1/empty.img:1", "--yes"],
    },
    Scenario {
        name: "set refuses an unknown property",
        argv: &["set", "target/fixtures/s1/gpt.img:1", "bogus", "x", "--yes"],
    },
    Scenario {
        name: "set refuses a filesystem label",
        argv: &[
            "set",
            "target/fixtures/s1/gpt.img:1",
            "label",
            "root",
            "--yes",
        ],
    },
    Scenario {
        name: "set label on ext refuses an over-long label",
        argv: &[
            "set",
            "target/fixtures/s1/ext.img:1",
            "label",
            "xxxxxxxxxxxxxxxxx",
            "--yes",
        ],
    },
    Scenario {
        name: "set uuid on ext refuses a malformed value",
        argv: &[
            "set",
            "target/fixtures/s1/ext.img:1",
            "uuid",
            "not-a-uuid",
            "--yes",
        ],
    },
    Scenario {
        name: "set uuid on unidentified content is refused",
        argv: &[
            "set",
            "target/fixtures/s1/gpt.img:1",
            "uuid",
            "4d4f4e4b-0000-4000-8000-000000000000",
            "--yes",
        ],
    },
    Scenario {
        name: "mkfs refuses an over-long label",
        argv: &[
            "mkfs",
            "target/fixtures/s1/ext.img:1",
            "ext4",
            "--label",
            "xxxxxxxxxxxxxxxxx",
            "--yes",
        ],
    },
    Scenario {
        name: "set name is a GPT-only property",
        argv: &["set", "target/fixtures/s1/mbr.img:1", "name", "x", "--yes"],
    },
];

/// 把本实现的命令行翻译成旧基线的等价命令行。
///
/// 旧二进制只作行为 oracle：新命令面是刻意重造的（`divergences.md` D20–D26），
/// 因此对照前必须经这一层适配，而不是把新 argv 原样喂给旧基线。翻译只覆盖
/// 一一对应的改名与形态合并；`None` 表示新面没有旧等价物，该条不参与对照。
fn legacy(argv: &[String]) -> Option<Vec<String>> {
    // 全局旗标改名：--alias → --identity（D25）。
    let mut tokens: Vec<String> = argv
        .iter()
        .map(|token| {
            if token == "--alias" {
                "--identity".to_owned()
            } else {
                token.clone()
            }
        })
        .collect();
    let command = tokens.first().map(String::as_str).unwrap_or_default();
    match command {
        // layout（--json 承载结构化明细）↔ info（旧版恒为 JSON，且不认识 --json）。
        "layout" => {
            tokens.remove(0);
            tokens.retain(|token| token != "--json");
            tokens.insert(0, "info".to_owned());
            Some(tokens)
        }
        // records show ↔ inspect（D21）。
        "records" if tokens.get(1).map(String::as_str) == Some("show") => {
            tokens.remove(0);
            tokens[0] = "inspect".to_owned();
            Some(tokens)
        }
        // records list --root ROOT ↔ records scan ROOT（D24）。
        "records"
            if tokens.get(1).map(String::as_str) == Some("list")
                && tokens.iter().any(|t| t == "--root") =>
        {
            let root = tokens
                .iter()
                .position(|token| token == "--root")
                .and_then(|at| tokens.get(at + 1).cloned());
            let mut reference = vec!["records".to_owned(), "scan".to_owned()];
            if let Some(root) = root {
                reference.push(root);
            }
            Some(reference)
        }
        // undo 是旧 recover 的默认回放；replay 对应 --replay-table-only；release 对应 --release（D22）。
        "undo" => {
            tokens[0] = "recover".to_owned();
            Some(tokens)
        }
        "replay" => {
            tokens[0] = "recover".to_owned();
            Some(reinsert_flag(tokens, "--replay-table-only", 1))
        }
        "release" => {
            tokens[0] = "recover".to_owned();
            Some(reinsert_flag(tokens, "--release", 1))
        }
        // resize --fs-only ↔ resizefs（D23）。
        "resize" if tokens.iter().any(|token| token == "--fs-only") => {
            tokens.remove(0);
            tokens.retain(|token| token != "--fs-only");
            tokens.insert(0, "resizefs".to_owned());
            Some(tokens)
        }
        // 旧 new 要求目标紧跟命令；本实现接受选项在目标前。对照时规范化为旧顺序。
        "new" if tokens.len() > 1 && !tokens.last().unwrap().starts_with("--") => {
            let target = tokens.pop().unwrap();
            tokens.insert(1, target);
            Some(tokens)
        }
        // add --dry-run 是旧版没有的预览通道（D26），无旧等价物。
        "add" if tokens.iter().any(|token| token == "--dry-run") => None,
        // --help 后带命令：旧版 --help 不接主题，等价于顶层帮助。
        "--help" if tokens.len() > 1 => Some(vec!["--help".to_owned()]),
        // help <topic>：主题按改名映射。
        "help" if tokens.get(1).map(String::as_str) == Some("layout") => {
            tokens[1] = "info".to_owned();
            Some(tokens)
        }
        _ => Some(tokens),
    }
}

/// 把 `flag` 插到位置 `after` 之后（旧 recover 的形态旗标紧跟目标）。
fn reinsert_flag(mut tokens: Vec<String>, flag: &str, after: usize) -> Vec<String> {
    let at = (after + 1).min(tokens.len());
    tokens.insert(at, flag.to_owned());
    tokens
}

/// 适配层自身的行为：改名、形态合并、无等价物必须逐条钉住，
/// 否则新旧对照会因为翻译错误而假绿。
#[test]
fn legacy_adapter_maps_the_renamed_surface() {
    let map = |argv: &[&str]| legacy(&owned(argv));
    assert_eq!(
        map(&["layout", "img", "--json"]),
        Some(owned(&["info", "img"]))
    );
    assert_eq!(
        map(&["records", "show", "img"]),
        Some(owned(&["inspect", "img"]))
    );
    assert_eq!(
        map(&["records", "list", "--root", "dir"]),
        Some(owned(&["records", "scan", "dir"]))
    );
    assert_eq!(
        map(&["records", "list", "--alias", "n", "img"]),
        Some(owned(&["records", "list", "--identity", "n", "img"]))
    );
    assert_eq!(
        map(&["undo", "img", "--yes"]),
        Some(owned(&["recover", "img", "--yes"]))
    );
    assert_eq!(
        map(&["replay", "img", "--yes"]),
        Some(owned(&["recover", "img", "--replay-table-only", "--yes"]))
    );
    assert_eq!(
        map(&["release", "img", "--discard-replay", "--yes"]),
        Some(owned(&[
            "recover",
            "img",
            "--release",
            "--discard-replay",
            "--yes"
        ]))
    );
    assert_eq!(
        map(&["resize", "img:1", "--fs-only", "--yes"]),
        Some(owned(&["resizefs", "img:1", "--yes"]))
    );
    assert_eq!(
        map(&["new", "--table", "mbr", "--yes", "img"]),
        Some(owned(&["new", "img", "--table", "mbr", "--yes"]))
    );
    assert_eq!(map(&["help", "layout"]), Some(owned(&["help", "info"])));
    assert_eq!(map(&["--help", "layout"]), Some(owned(&["--help"])));
    // 旧版没有预览通道：不伪造对照。
    assert_eq!(map(&["add", "img", "1M", "--dry-run"]), None);
    // 未改名、未变更形态的命令原样透传。
    assert_eq!(
        map(&["add", "img", "1M", "--yes"]),
        Some(owned(&["add", "img", "1M", "--yes"]))
    );
}

fn code(program: &std::path::Path, argv: &[&str]) -> i32 {
    Command::new(program)
        .env("XDG_STATE_HOME", fixtures::STATE_HOME)
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {} 失败: {error}", program.display()))
        .status
        .code()
        .expect("子进程应正常退出而不是被信号终止")
}

fn code_owned(program: &std::path::Path, argv: &[String]) -> i32 {
    Command::new(program)
        .env("XDG_STATE_HOME", fixtures::STATE_HOME)
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {} 失败: {error}", program.display()))
        .status
        .code()
        .expect("子进程应正常退出而不是被信号终止")
}

/// 两份字节之间所有不同的扇区；按扇区比较，GUID 等随机内容的逐字节差异不参与。
fn changed_offsets(before: &[u8], after: &[u8]) -> Vec<usize> {
    let sector = 512;
    let common = before.len().min(after.len());
    let mut sectors = std::collections::BTreeSet::new();
    for at in 0..common {
        if before[at] != after[at] {
            sectors.insert(at / sector);
        }
    }
    if after.len() > common {
        sectors.insert(common / sector);
    }
    sectors.into_iter().collect()
}

fn capture(program: &std::path::Path, argv: &[&str]) -> std::process::Output {
    Command::new(program)
        .env("XDG_STATE_HOME", fixtures::STATE_HOME)
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {} 失败: {error}", program.display()))
}

fn capture_owned(program: &std::path::Path, argv: &[String]) -> std::process::Output {
    Command::new(program)
        .env("XDG_STATE_HOME", fixtures::STATE_HOME)
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {} 失败: {error}", program.display()))
}

fn owned(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|token| (*token).to_owned()).collect()
}

// covers: D9, D16, D18, D20
// 旧基线仍是旧命令面：对照前先经 `legacy` 适配（改名/形态合并），无等价物的条目跳过。
#[test]
fn both_binaries_agree_on_layout_json_fields() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，JSON 字段对照未执行");
        return;
    };
    let baseline = std::path::PathBuf::from(baseline);
    let candidate = std::path::PathBuf::from(env!("CARGO_BIN_EXE_diskedit"));
    fixtures::prepare();
    let cases: &[&[&str]] = &[
        &["layout", "target/fixtures/s1/gpt.img", "--json"],
        &["layout", "target/fixtures/s1/mbr.img", "--json"],
        &["layout", "target/fixtures/s1/empty.img", "--json"],
        &["layout", "target/fixtures/s1/damaged.img", "--json"],
        &["layout", "target/fixtures/s1/gpt.img:1", "--json"],
        &["add", "target/fixtures/s1/gpt.img", "1M", "--dry-run"],
        &[
            "layout",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "4096",
            "--json",
        ],
        &[
            "layout",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "123",
            "--json",
        ],
    ];
    for argv in cases {
        // add --dry-run 是旧版没有的预览通道（D26）；没有 oracle 就不假装对照。
        let Some(reference) = legacy(&owned(argv)) else {
            continue;
        };
        let base = capture_owned(&baseline, &reference);
        let cand = capture(&candidate, argv);
        assert_eq!(
            cand.status.code(),
            base.status.code(),
            "{argv:?} 的退出码不一致"
        );
        if base.status.code() != Some(0) {
            continue;
        }
        let expected: serde_json::Value =
            serde_json::from_slice(&base.stdout).unwrap_or_else(|error| {
                panic!("基线 {argv:?} 的 stdout 不是 JSON：{error}");
            });
        let actual: serde_json::Value =
            serde_json::from_slice(&cand.stdout).unwrap_or_else(|error| {
                panic!("候选 {argv:?} 的 stdout 不是 JSON：{error}");
            });
        assert_eq!(actual, expected, "{argv:?} 的 layout JSON 不一致");
    }
}

/// 写场景必须各用自己的副本：两个二进制看到同一初始字节，互不污染。
#[test]
fn both_binaries_agree_on_write_scenarios() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，写场景对照未执行");
        return;
    };
    let baseline = std::path::PathBuf::from(baseline);
    let candidate = std::path::PathBuf::from(env!("CARGO_BIN_EXE_diskedit"));
    fixtures::prepare();
    let cases: &[(&str, &str, &[&str])] = &[
        ("new gpt", "blank.img", &["new", "--yes"]),
        ("new mbr", "blank.img", &["new", "--table", "mbr", "--yes"]),
        ("new occupied refuses", "data.img", &["new", "--yes"]),
        (
            "new occupied overwrites",
            "data.img",
            &["new", "--yes", "--overwrite-data"],
        ),
        ("new needs yes", "blank.img", &["new"]),
        (
            "new on a sector-less image",
            "../s1/tiny.img",
            &["new", "--yes"],
        ),
    ];
    for (index, (name, source, prefix)) in cases.iter().enumerate() {
        let original = std::path::Path::new(fixtures::S2_BASE).join(source);
        let before = std::fs::read(&original).expect("读取夹具");
        let for_baseline = format!("{}/wr-{index}-base.img", fixtures::S2_BASE);
        let for_candidate = format!("{}/wr-{index}-new.img", fixtures::S2_BASE);
        std::fs::write(&for_baseline, &before).expect("写基线副本");
        std::fs::write(&for_candidate, &before).expect("写候选副本");
        let argv = |target: &str| {
            let mut argv: Vec<String> = prefix.iter().map(|part| (*part).to_owned()).collect();
            argv.push(target.to_owned());
            argv
        };
        let expected = code_owned(
            &baseline,
            &legacy(&argv(&for_baseline)).expect("写场景都有旧等价物"),
        );
        let actual = code_owned(&candidate, &argv(&for_candidate));
        assert_eq!(actual, expected, "{name} 的退出码不一致");
        let after_base = std::fs::read(&for_baseline).expect("读取基线副本");
        let after_candidate = std::fs::read(&for_candidate).expect("读取候选副本");
        assert_eq!(
            changed_offsets(&before, &after_candidate),
            changed_offsets(&before, &after_base),
            "{name} 改动的字节位置不一致"
        );
    }
}

#[test]
fn both_binaries_agree_on_table_edits() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，表编辑对照未执行");
        return;
    };
    let baseline = std::path::PathBuf::from(baseline);
    let candidate = std::path::PathBuf::from(env!("CARGO_BIN_EXE_diskedit"));
    fixtures::prepare();
    let cases: &[(&str, &str, &[&str])] = &[
        ("add gpt", "gpt.img", &["add", "{T}", "1M", "--yes"]),
        (
            "add gpt explicit start",
            "gpt.img",
            &["add", "{T}", "300K", "--start", "5000", "--yes"],
        ),
        ("add mbr", "mbr.img", &["add", "{T}", "512K", "--yes"]),
        ("delete gpt", "gpt.img", &["delete", "{T}:2", "--yes"]),
        ("delete mbr", "mbr.img", &["delete", "{T}:1", "--yes"]),
        (
            "set gpt name",
            "gpt.img",
            &["set", "{T}:1", "name", "rootfs", "--yes"],
        ),
        (
            "set gpt type",
            "gpt.img",
            &[
                "set",
                "{T}:2",
                "type",
                "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
                "--yes",
            ],
        ),
        (
            "set gpt flag",
            "gpt.img",
            &["set", "{T}:1", "flag", "hidden", "on", "--yes"],
        ),
        (
            "set mbr boot",
            "mbr.img",
            &["set", "{T}:1", "flag", "boot", "on", "--yes"],
        ),
        (
            "set mbr type",
            "mbr.img",
            &["set", "{T}:1", "type", "0x0c", "--yes"],
        ),
    ];
    for (index, (name, source, template)) in cases.iter().enumerate() {
        let original = std::path::Path::new(fixtures::BASE).join(source);
        let before = std::fs::read(&original).expect("读取夹具");
        let for_baseline = format!("{}/tb-{index}-base.img", fixtures::S2_BASE);
        let for_candidate = format!("{}/tb-{index}-new.img", fixtures::S2_BASE);
        std::fs::write(&for_baseline, &before).expect("写基线副本");
        std::fs::write(&for_candidate, &before).expect("写候选副本");
        let argv = |target: &str| {
            template
                .iter()
                .map(|part| part.replace("{T}", target))
                .collect::<Vec<String>>()
        };
        let expected = code_owned(
            &baseline,
            &legacy(&argv(&for_baseline)).expect("表编辑都有旧等价物"),
        );
        let actual = code_owned(&candidate, &argv(&for_candidate));
        assert_eq!(actual, expected, "{name} 的退出码不一致");
        let after_base = std::fs::read(&for_baseline).expect("读取基线副本");
        let after_candidate = std::fs::read(&for_candidate).expect("读取候选副本");
        assert_eq!(
            changed_offsets(&before, &after_candidate),
            changed_offsets(&before, &after_base),
            "{name} 改动的字节位置不一致"
        );
    }
}

#[test]
fn both_binaries_agree_on_every_scenario() {
    let Some(baseline) = std::env::var_os("DISKEDIT_BASELINE") else {
        eprintln!("NotApplicable: 未提供 DISKEDIT_BASELINE，等价对照未执行");
        return;
    };
    let baseline = std::path::PathBuf::from(baseline);
    let candidate = std::path::PathBuf::from(env!("CARGO_BIN_EXE_diskedit"));
    fixtures::prepare();

    for scenario in SCENARIOS {
        let Some(reference) = legacy(&owned(scenario.argv)) else {
            // 新面没有旧等价物（如 add --dry-run），由候选侧用例覆盖。
            continue;
        };
        let expected = code_owned(&baseline, &reference);
        let actual = code(&candidate, scenario.argv);
        assert_eq!(
            actual, expected,
            "{} 的退出码不一致：基线 {expected}，本实现 {actual}",
            scenario.name
        );
    }
}
