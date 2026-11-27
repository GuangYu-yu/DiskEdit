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

// covers: D9
// 注：对照的基线二进制仍是旧命令面；改名后该对照只对未改名的条目有意义。
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
        let base = capture(&baseline, argv);
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
        let expected = code_owned(&baseline, &argv(&for_baseline));
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
        let expected = code_owned(&baseline, &argv(&for_baseline));
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
        let expected = code(&baseline, scenario.argv);
        let actual = code(&candidate, scenario.argv);
        assert_eq!(
            actual, expected,
            "{} 的退出码不一致：基线 {expected}，本实现 {actual}",
            scenario.name
        );
    }
}
