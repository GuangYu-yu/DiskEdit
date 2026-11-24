//! 等价对照：同一 argv、同一现场，两个二进制比对退出码。
//!
//! 基线二进制由 `DISKEDIT_BASELINE` 指定；未提供时回报 `NotApplicable`，不静默跳过。
//! S0 只覆盖进程级出口（帮助、版本、用法）；命令行为按纵切加入对照。

use std::process::Command;

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
