//! 帮助与参考输出。
//!
//! 没有 `reference` 子命令，也没有任何命令打印能力矩阵：对外可见的“参考”
//! 只有顶层 usage 和每条命令的 help，能力矩阵的可见形态是命令是否登记
//! 与退出码 6 的平台闸门。
//!
//! 顶层 usage 按当前构建的文本做逐字快照，当前示例是
//! `resize disk.img grow --allow-move --chunk 4 --yes`。快照失败时先核对改动是否
//! 符合帮助与校验同源的要求，不要为让快照通过而迁就错误文本。每条命令的帮助做结构
//! 断言：只解析语义字段，不比对整段输出。
//!
//! 帮助请求在平台闸门之前就返回，因此本文件在 Windows 与 Linux 都运行。

use std::collections::BTreeSet;
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn Diskedit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// usage 文本里的命令面：顶层命令名，以及 `records` 下的子命令名。
///
/// 判据只看行的形状：两个空格的缩进之后紧跟一个纯字母串。`diskedit ...` 是调用串，
/// `--option`、更深的缩进与 `  0  success` 这类退出码行都不合形状。`records <sub>`
/// 的第二个词记作 `records` 的子命令，顶层集合里只留 `records`。
fn parse_usage_surface(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut commands = BTreeSet::new();
    let mut records = BTreeSet::new();
    for line in text.lines() {
        let Some(body) = line.strip_prefix("  ") else { continue };
        if body.starts_with(' ') {
            continue;
        }
        let mut words = body.split_whitespace();
        let Some(name) = words.next() else { continue };
        if name == "diskedit" || !name.bytes().all(|b| b.is_ascii_alphabetic()) {
            continue;
        }
        commands.insert(name.to_string());
        if name != "records" {
            continue;
        }
        if let Some(sub) = words.next().filter(|w| w.bytes().all(|b| b.is_ascii_alphabetic())) {
            records.insert(sub.to_string());
        }
    }
    (commands, records)
}

/// usage 里 `  0  success` 这类退出码行的码值：两个空格、一位 0..=6、再两个空格
fn exit_codes(text: &str) -> BTreeSet<u8> {
    let mut codes = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        let bytes = rest.as_bytes();
        if bytes.len() > 3 && (b'0'..=b'6').contains(&bytes[0]) && bytes[1] == b' ' && bytes[2] == b' ' {
            codes.insert(bytes[0] - b'0');
        }
    }
    codes
}

/// 顶层 usage 的逐字快照：`help` / `--help` / `-h` 的 stdout，与
/// `tests/snapshots/usage.txt` 逐字节相等（该文件由二进制 `help` 重录，不是手抄）。
///
/// 帮助文本由命令表渲染。快照不得出现 `cyl`/CHS；
/// 文本随命令表或示例变化时，先核对新文本是否仍满足要求，再从二进制重新抓取。
const USAGE_SNAPSHOT: &str = include_str!("snapshots/usage.txt");

#[test]
fn top_level_usage_is_a_pinned_snapshot() {
    for args in [vec!["help"], vec!["--help"], vec!["-h"]] {
        let (code, out, err) = run(&args);
        assert_eq!(code, 0, "`{}` must exit 0: {err}", args.join(" "));
        assert!(err.is_empty(), "`{}` writes to stdout only: {err}", args.join(" "));
        assert_eq!(
            out,
            USAGE_SNAPSHOT,
            "`{}` drifted from the pinned usage text",
            args.join(" ")
        );
    }
}

#[test]
fn usage_lists_the_command_surface_and_the_full_exit_legend() {
    let (code, out, err) = run(&["help"]);
    assert_eq!(code, 0, "`help` must exit 0: {err}");
    assert!(err.is_empty(), "`help` writes to stdout only: {err}");

    let (commands, records) = parse_usage_surface(&out);
    assert!(!commands.is_empty(), "usage parsed no command lines:\n{out}");
    assert!(!records.is_empty(), "usage parsed no records subcommands:\n{out}");
    let legend: BTreeSet<u8> = (0..=6).collect();
    assert_eq!(
        exit_codes(&out),
        legend,
        "usage must list each exit code 0..=6:\n{out}"
    );
    for absent in ["  apply", "records prune"] {
        assert!(
            !out.contains(absent),
            "usage advertises the non-target command {absent:?}:\n{out}"
        );
    }
}

/// 命令面读的是喂进来的文本：样本少一行，派生集合就少一个名字
#[test]
fn the_usage_surface_follows_the_text_it_is_given() {
    let full = "usage:\n  info <TARGET>\n  records list [TARGET]\n  records verify <TARGET> <ID>\n";
    let (commands, records) = parse_usage_surface(full);
    assert!(commands.contains("info"), "sample commands: {commands:?}");
    assert!(records.contains("list"), "sample records subcommands: {records:?}");

    let short = "usage:\n  records list [TARGET]\n";
    let (commands, records) = parse_usage_surface(short);
    assert!(
        !commands.contains("info"),
        "the surface kept a command the text does not list: {commands:?}"
    );
    assert!(
        !records.contains("verify"),
        "the surface kept a records subcommand the text does not list: {records:?}"
    );
}

#[test]
fn every_help_entry_point_renders_the_same_text() {
    let (commands, records) = parse_usage_surface(&run(&["help"]).1);
    for name in commands.iter().filter(|n| n.as_str() != "records").map(String::as_str) {
        let canonical = run(&["help", name]);
        assert!(!canonical.1.is_empty(), "`help {name}` printed no help");
        assert_eq!(canonical.0, 0, "`help {name}` must exit 0");
        assert!(canonical.2.is_empty(), "`help {name}` must not write stderr");

        for args in [vec![name, "--help"], vec![name, "-h"], vec!["--help", name]] {
            let got = run(&args);
            assert_eq!(got.0, 0, "`{}` must exit 0", args.join(" "));
            assert_eq!(
                got.1, canonical.1,
                "`{}` must render the same help as `help {name}`",
                args.join(" ")
            );
        }
        assert!(
            canonical.1.starts_with(&format!("diskedit {name}")),
            "`help {name}` must start with its invocation:\n{}",
            canonical.1
        );
    }

    for sub in &records {
        let topic = format!("records {sub}");
        let canonical = run(&["help", "records", sub.as_str()]);
        assert_eq!(canonical.0, 0, "`help {topic}` must exit 0");
        assert!(canonical.2.is_empty(), "`help {topic}` must not write stderr");
        assert!(
            canonical.1.starts_with(&format!("diskedit {topic}")),
            "`help {topic}` must start with its invocation:\n{}",
            canonical.1
        );

        for args in [vec!["records", sub.as_str(), "--help"], vec!["records", sub.as_str(), "-h"]] {
            let got = run(&args);
            assert_eq!(got.0, 0, "`{}` must exit 0", args.join(" "));
            assert_eq!(
                got.1, canonical.1,
                "`{}` must render the same help as `help {topic}`",
                args.join(" ")
            );
        }
    }
}

#[test]
fn help_never_mentions_cylinder_or_chs_geometry() {
    let (commands, records) = parse_usage_surface(&run(&["help"]).1);
    let mut texts = vec![run(&["help"]).1];
    for name in commands.iter().filter(|n| n.as_str() != "records").map(String::as_str) {
        texts.push(run(&["help", name]).1);
    }
    for sub in &records {
        texts.push(run(&["help", "records", sub.as_str()]).1);
    }
    for text in texts {
        let lower = text.to_lowercase();
        for forbidden in ["cyl", "chs"] {
            assert!(
                !lower.contains(forbidden),
                "help mentions {forbidden:?} geometry:\n{text}"
            );
        }
    }
}

#[test]
fn unknown_help_topics_and_unknown_commands_are_usage_errors() {
    let (code, out, err) = run(&["help", "frobnicate"]);
    assert_eq!(code, 2, "an unknown help topic is a usage error");
    assert!(out.is_empty(), "a usage error must not write stdout: {out}");
    assert!(err.contains("usage:"), "a usage error must print usage: {err}");

    let (code, out, err) = run(&["frobnicate"]); // cli-surface: refused
    assert_eq!(code, 2, "an unknown command is a usage error");
    assert!(out.is_empty(), "a usage error must not write stdout: {out}");
    assert!(err.contains("usage:"), "a usage error must print usage: {err}");
}

#[test]
fn help_documents_the_registered_flags_of_the_contract() {
    // 只断言明确登记、且帮助正文承载的旗标；不反推“帮助里没写的旗标不合法”。
    let cases: &[(&str, &[&str])] = &[
        ("info", &["--sector-size", "--grow"]),
        ("new", &["--table", "--overwrite-data", "--sector-size"]),
        ("add", &["--start", "--align", "--part-number", "--name", "--type-guid"]),
        ("set", &["--random"]),
        ("mkfs", &["--label"]),
        ("recover", &["--release", "--discard-replay", "--yes"]),
    ];
    for (name, flags) in cases {
        let (code, out, err) = run(&["help", name]);
        assert_eq!(code, 0, "`help {name}` must exit 0: {err}");
        for flag in *flags {
            assert!(
                out.contains(flag),
                "`help {name}` does not document the registered flag {flag:?}:\n{out}"
            );
        }
    }
}
