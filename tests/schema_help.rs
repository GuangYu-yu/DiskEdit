//! §8 阶段 5 的验收入口：帮助来自同一张命令表，入口一致，且不再出现柱面几何。
use std::process::Command;

/// 每条命令按词拆开：`records list` 这类双词命令的每个词都是一个参数
const COMMANDS: &[&[&str]] = &[
    &["info"],
    &["inspect"],
    &["records", "list"],
    &["records", "verify"],
    &["records", "scan"],
    &["resize"],
    &["move"],
    &["copy"],
    &["delete"],
    &["set"],
    &["check"],
    &["mkfs"],
    &["resizefs"],
    &["undo"],
    &["abandon"],
    &["new"],
    &["add"],
    &["resize-part"],
    &["plan"],
];

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn usage_lists_every_command(text: &str) {
    for cmd in COMMANDS {
        let name = cmd.join(" ");
        assert!(
            text.contains(&format!("\n  {name} ")),
            "usage never lists `{name}`:\n{text}"
        );
    }
}

/// 每条命令都有详助，首行自报命令名；`help CMD`、`CMD --help`、`CMD -h`、`--help CMD`
/// 四个入口都要给出同一份文本
#[test]
fn every_command_has_detail_help_naming_itself() {
    for cmd in COMMANDS {
        let name = cmd.join(" ");
        let mut ask: Vec<&str> = vec!["help"];
        ask.extend_from_slice(cmd);
        let (code, out, err) = run(&ask);
        assert_eq!(code, 0, "help {name}: {err}");
        let first = out.lines().next().unwrap_or("");
        assert!(
            first.starts_with(&format!("diskedit {name}")),
            "help {name} starts with {first:?}"
        );

        for suffix in [["--help"], ["-h"]] {
            let mut alternate: Vec<&str> = cmd.to_vec();
            alternate.extend_from_slice(&suffix);
            let (c, o, e) = run(&alternate);
            assert_eq!(c, 0, "{alternate:?}: {e}");
            assert_eq!(o, out, "{alternate:?} must print the same detail help as `help {name}`");

            let mut prefixed: Vec<&str> = suffix.to_vec();
            prefixed.extend_from_slice(cmd);
            let (c, o, e) = run(&prefixed);
            assert_eq!(c, 0, "{prefixed:?}: {e}");
            assert_eq!(o, out, "{prefixed:?} must print the same detail help as `help {name}`");
        }
    }
}

/// 顶层用法把全部命令列出来；主动请求帮助退 0，不写命令退 2 且原因在 stderr
#[test]
fn top_level_usage_lists_every_command() {
    let (code, out, err) = run(&["help"]);
    assert_eq!(code, 0, "help: {err}");
    usage_lists_every_command(&out);

    for flag in ["--help", "-h"] {
        let (c, o, e) = run(&[flag]);
        assert_eq!(c, 0, "{flag}: {e}");
        assert_eq!(o, out, "{flag} and `help` must print the same usage text");
    }

    let (c, _, e) = run(&[]);
    assert_eq!(c, 2, "a missing command is a usage error");
    assert!(!e.is_empty(), "the usage error must go to stderr");
    usage_lists_every_command(&e);
}

/// 帮助里不再出现柱面对齐或 CHS 几何
#[test]
fn help_output_has_no_cylinder_geometry() {
    let mut all = run(&["help"]).1;
    for cmd in COMMANDS {
        let mut ask: Vec<&str> = vec!["help"];
        ask.extend_from_slice(cmd);
        all.push_str(&run(&ask).1);
    }
    let lower = all.to_ascii_lowercase();
    assert!(!lower.contains("cyl"), "help still advertises cylinder alignment");
    assert!(!lower.contains("chs"), "help still advertises CHS geometry");
}

/// 未知帮助主题走顶层用法，退 2
#[test]
fn unknown_help_topic_is_a_usage_error() {
    let (code, _, err) = run(&["help", "frobnicate"]);
    assert_eq!(code, 2);
    assert!(!err.is_empty(), "the refusal must say why");
}
