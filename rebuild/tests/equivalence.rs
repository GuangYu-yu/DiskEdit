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
        argv: &["help", "info"],
    },
    Scenario {
        name: "help with a two-word topic",
        argv: &["help", "records", "list"],
    },
    Scenario {
        name: "help flag after a command",
        argv: &["info", "--help"],
    },
    Scenario {
        name: "help flag before a command",
        argv: &["--help", "info"],
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
        argv: &["info"],
    },
    Scenario {
        name: "unknown option",
        argv: &["info", "--bogus"],
    },
    Scenario {
        name: "info on a GPT image",
        argv: &["info", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "info on an MBR image",
        argv: &["info", "target/fixtures/s1/mbr.img"],
    },
    Scenario {
        name: "info on an empty image",
        argv: &["info", "target/fixtures/s1/empty.img"],
    },
    Scenario {
        name: "info on a damaged GPT image",
        argv: &["info", "target/fixtures/s1/damaged.img"],
    },
    Scenario {
        name: "info with a partition and no grow",
        argv: &["info", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "info grow on the last partition",
        argv: &["info", "target/fixtures/s1/gpt.img:2", "--grow"],
    },
    Scenario {
        name: "info grow on the first partition",
        argv: &["info", "target/fixtures/s1/gpt.img:1", "--grow"],
    },
    Scenario {
        name: "info grow on MBR",
        argv: &["info", "target/fixtures/s1/mbr.img:1", "--grow"],
    },
    Scenario {
        name: "info with an invalid sector size",
        argv: &["info", "target/fixtures/s1/gpt.img", "--sector-size", "123"],
    },
    Scenario {
        name: "info with a 4096 sector size",
        argv: &[
            "info",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "4096",
        ],
    },
    Scenario {
        name: "info on a missing target",
        argv: &["info", "target/fixtures/s1/missing.img"],
    },
    Scenario {
        name: "inspect",
        argv: &["inspect", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "inspect on a missing target",
        argv: &["inspect", "target/fixtures/s1/missing.img"],
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
        name: "records scan a directory",
        argv: &["records", "scan", "target/fixtures/s1"],
    },
    Scenario {
        name: "records scan a missing directory",
        argv: &["records", "scan", "target/fixtures/s1/missing"],
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
        name: "info on a target smaller than one sector",
        argv: &["info", "target/fixtures/s1/tiny.img"],
    },
    Scenario {
        name: "info on an empty file",
        argv: &["info", "target/fixtures/s1/zero.img"],
    },
    Scenario {
        name: "info grow without a partition selector",
        argv: &["info", "--grow", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "info with a missing option value",
        argv: &["info", "target/fixtures/s1/gpt.img", "--sector-size"],
    },
    Scenario {
        name: "info with a repeated value option",
        argv: &[
            "info",
            "target/fixtures/s1/gpt.img",
            "--sector-size",
            "512",
            "--sector-size",
            "4096",
        ],
    },
    Scenario {
        name: "info with a value on a bare flag",
        argv: &["info", "--grow=1", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "info with an extra argument",
        argv: &["info", "target/fixtures/s1/gpt.img", "extra"],
    },
    Scenario {
        name: "inspect without a target",
        argv: &["inspect"],
    },
    Scenario {
        name: "inspect refuses a partition selector",
        argv: &["inspect", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "records list refuses a partition selector",
        argv: &["records", "list", "target/fixtures/s1/gpt.img:1"],
    },
    Scenario {
        name: "records list identity without a target",
        argv: &["records", "list", "--identity", "declared-name"],
    },
    Scenario {
        name: "records list identity on an image",
        argv: &[
            "records",
            "list",
            "--identity",
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
        name: "records scan without a root",
        argv: &["records", "scan"],
    },
    Scenario {
        name: "records scan refuses a file",
        argv: &["records", "scan", "target/fixtures/s1/gpt.img"],
    },
    Scenario {
        name: "check on a directory",
        argv: &["check", "target/fixtures/s1:1"],
    },
];

fn code(program: &std::path::Path, argv: &[&str]) -> i32 {
    Command::new(program)
        .args(argv)
        .output()
        .unwrap_or_else(|error| panic!("运行 {} 失败: {error}", program.display()))
        .status
        .code()
        .expect("子进程应正常退出而不是被信号终止")
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
