//! 帮助与参考输出。
//!
//! 没有 `reference` 子命令，也没有任何命令打印能力矩阵：对外可见的“参考”
//! 只有顶层 usage 和每条命令的 help，能力矩阵的可见形态是命令是否登记
//! 与退出码 6 的平台闸门。
//!
//! 顶层 usage 按当前构建的文本做逐字快照；**文本可能随主线修示例而变化**（本快照已
//! 跟随当前示例 `resize disk.img grow --allow-move --chunk 4 --yes`）。快照失败时先核对
//! 改动是否符合帮助与校验同源的要求，不要为让快照通过而迁就错误文本。每条命令的帮助做结构
//! 断言：只解析语义字段，不比对整段输出。
//!
//! 帮助请求在平台闸门之前就返回，因此本文件在 Windows 与 Linux 都运行。

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

/// 命令集在 usage 里的行首片段。
const USAGE_COMMANDS: &[&str] = &[
    "  info <TARGET>",
    "  plan <TARGET>",
    "  inspect <TARGET>",
    "  records list [TARGET]",
    "  records verify <ID> <TARGET>",
    "  records scan <ROOT>",
    "  resize <TARGET>",
    "  move <TARGET>:N",
    "  copy <TARGET>:N",
    "  delete <TARGET>:N",
    "  set <TARGET>:N",
    "  check <TARGET>:N",
    "  mkfs <TARGET>:N",
    "  resizefs <TARGET>:N",
    "  undo <TARGET>",
    "  abandon <TARGET>",
    "  new <TARGET>",
    "  add <TARGET>",
    "  resize-part <TARGET>:N",
];

/// 退出码表。
const EXIT_LINES: &[&str] = &[
    "  0  success",
    "  1  unexpected internal error",
    "  2  usage or command syntax error",
    "  3  policy refusal, or a precondition of the request is not met",
    "  4  operation failed with no persistent modification",
    "  5  partial, or applied with follow-up steps pending or a stale kernel view (the remedy is printed)",
    "  6  the platform or capability is not supported",
];

/// 顶层 usage 的逐字快照：`help` / `--help` / `-h` 的 stdout，与
/// `tests/snapshots/usage.txt` 逐字节相等（该文件由二进制 `help` 重录，不是手抄）。
///
/// 帮助文本由命令表渲染（`docs/reference.md` 同源）。快照不得出现 `cyl`/CHS；
/// 文本随命令表或示例变化时，先核对新文本是否仍满足要求，再从二进制重新抓取。
const USAGE_SNAPSHOT: &str = include_str!("snapshots/usage.txt");

/// 单字命令：`help CMD`、`CMD --help`、`CMD -h`、`--help CMD` 四种入口。
const SINGLE_WORD: &[&str] = &[
    "info",
    "plan",
    "inspect",
    "resize",
    "move",
    "copy",
    "delete",
    "set",
    "check",
    "mkfs",
    "resizefs",
    "undo",
    "abandon",
    "new",
    "add",
    "resize-part",
];

/// 两字命令：`help records X`、`records X --help`、`records X -h` 三种入口。
const RECORD_SUBCOMMANDS: &[&[&str]] = &[&["records", "list"], &["records", "verify"], &["records", "scan"]];

#[test]
fn top_level_usage_is_a_pinned_snapshot() {
    // `help`、`--help`、`-h` 都是元命令，按成功返回且输出同一份用法文本
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
fn usage_lists_the_contract_command_set_and_exit_legend() {
    let (code, out, err) = run(&["help"]);
    assert_eq!(code, 0, "`help` must exit 0: {err}");
    assert!(err.is_empty(), "`help` writes to stdout only: {err}");
    for needle in USAGE_COMMANDS {
        assert!(
            out.contains(needle),
            "usage is missing the contract command line {needle:?}:\n{out}"
        );
    }
    for line in EXIT_LINES {
        assert!(
            out.contains(line),
            "usage is missing the exit-status line {line:?}:\n{out}"
        );
    }
    for absent in ["  apply", "records prune"] {
        assert!(
            !out.contains(absent),
            "usage advertises the non-target command {absent:?}:\n{out}"
        );
    }
}

#[test]
fn every_help_entry_point_renders_the_same_text() {
    for name in SINGLE_WORD {
        let canonical = run(&["help", name]);
        assert!(!canonical.1.is_empty(), "`help {name}` printed no help");
        assert_eq!(canonical.0, 0, "`help {name}` must exit 0");
        assert!(canonical.2.is_empty(), "`help {name}` must not write stderr");

        for args in [vec![*name, "--help"], vec![*name, "-h"], vec!["--help", name]] {
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

    for sub in RECORD_SUBCOMMANDS {
        let topic = sub.join(" ");
        let canonical = run(&["help", sub[0], sub[1]]);
        assert_eq!(canonical.0, 0, "`help {topic}` must exit 0");
        assert!(canonical.2.is_empty(), "`help {topic}` must not write stderr");
        assert!(
            canonical.1.starts_with(&format!("diskedit {topic}")),
            "`help {topic}` must start with its invocation:\n{}",
            canonical.1
        );

        for args in [vec![sub[0], sub[1], "--help"], vec![sub[0], sub[1], "-h"]] {
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
    let mut texts = vec![run(&["help"]).1];
    for name in SINGLE_WORD {
        texts.push(run(&["help", name]).1);
    }
    for sub in RECORD_SUBCOMMANDS {
        texts.push(run(&["help", sub[0], sub[1]]).1);
    }
    for text in texts {
        let lower = text.to_lowercase();
        for forbidden in ["cyl", "chs"] {
            assert!(
                !lower.contains(forbidden),
                "help mentions legacy {forbidden:?} geometry:\n{text}"
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

    let (code, out, err) = run(&["frobnicate"]); // cli-surface: refused —— 断言未登记命令退 2
    assert_eq!(code, 2, "an unknown command is a usage error");
    assert!(out.is_empty(), "a usage error must not write stdout: {out}");
    assert!(err.contains("usage:"), "a usage error must print usage: {err}");
}

#[test]
fn help_documents_the_registered_flags_of_the_contract() {
    // 只断言明确登记、且帮助正文承载的旗标；不反推“帮助里没写的旗标不合法”。
    let cases: &[(&str, &[&str])] = &[
        ("info", &["--sector-size"]),
        ("plan", &["--grow"]),
        ("new", &["--table", "--overwrite-data", "--sector-size"]),
        ("add", &["--start", "--align", "--part-number", "--name", "--type-guid"]),
        ("set", &["--random"]),
        ("mkfs", &["--label"]),
        ("abandon", &["--discard-replay"]),
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
