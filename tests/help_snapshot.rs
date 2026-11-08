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
    "  5  partially completed, or recovery is pending (the remedy is printed)",
    "  6  the platform or capability is not supported",
];

/// 顶层 usage 的逐字快照：`help` / `--help` / `-h` 的 stdout，去掉 `println!` 的尾换行。
///
/// 帮助文本由命令表渲染（`docs/reference.md` 同源）。本常量不是手抄：取二进制
/// `help` 的实际 stdout 重录。帮助与校验须同源，且快照不得出现 `cyl`/CHS；
/// 文本随命令表或示例变化时，先核对新文本是否仍满足要求，再从二进制重新抓取。
const USAGE_SNAPSHOT: &str = r#"diskedit — disk / image editor

usage:
  diskedit <COMMAND> [ARGS]                details for one command: diskedit help <CMD>
  diskedit --version

examples:
  diskedit info /dev/sdb
  diskedit plan /dev/sdb:2 --grow
  diskedit resize /dev/sdb:1 +2G
  diskedit resize disk.img grow --allow-move --chunk 4 --yes
  diskedit undo /dev/sdb --yes

everyday:
  info <TARGET> [--sector-size N]
      show partition table and per-partition filesystem identification
  inspect <TARGET>
  inspect <PATH>
      show the recovery record beside a target (read-only)
  records list [TARGET]
      list recovery records as JSON (omit TARGET: everything under
      <state_root>)
  records verify <ID> <TARGET>
      re-check one record by its 32-hex id
  records scan <ROOT>
      enumerate records under a container root (<state_root>/recovery, or an
      image directory)
  resize <TARGET>[:N] <SIZE> [OPTIONS]
      resize partition + FS (auto online/offline)
  move <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A]
      move a partition, data follows (chunked copy, resumable)
  copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
      copy a partition to a new location; the source is untouched
  delete <TARGET>:N --yes
      delete a partition entry (the data area is not wiped)
  set <TARGET>:N name S | label S | uuid U | flag F on|off --yes
  set <TARGET>:N type GUID | type 0xXX --yes
      set a partition property (name / label / uuid / flag / type)
  check <TARGET>:N
      check filesystem consistency without writing
  mkfs <TARGET>:N <FS> --yes [--label S]
      create a filesystem (destroys all data on the partition)
  resizefs <TARGET>:N --yes
  resizefs <MOUNTPOINT> --yes [BYTES]
      resize a filesystem only — the partition entry stays put
  undo <TARGET> --yes
      roll back the writes this tool completed (replays its record)
  abandon <TARGET>
      drop an unfinished operation's recovery files; no rollback
  plan <TARGET>[:N] [--grow] [--sector-size N]
      read-only report: geometry, table, FS and usage — the same fields as
      `info`; --grow appends a relocation preview

low-level (one step per run; long jobs stay resumable):
  new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
      create a fresh partition table
  add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes
      add a partition; --start fixes where it begins
  resize-part <TARGET>:N (--end LBA | --grow-to-end) [--align A] --yes
      repartition one partition in place

target: image path or block device; :N = partition number (1-based). :last = the last
        partition a command can act on. On MBR an extended container does not count
        as one; a target without a partition table has no :last

options (availability: help <CMD>):
  --align A              placement alignment: mib|none. add's automatic
                         placement defaults to 1 MiB; an explicit --start and
                         move/copy/resize-part align only when --align is
                         given
  --allow-move           permit relocating the partitions in the way: the plan
                         is printed, then the write proceeds
  --chunk MiB            recovery granularity in MiB, 1..=64 (default 4); an
                         interrupted run must repeat the same value
  --discard-replay       confirm giving up the ranges `undo` could still
                         replay
  --end LBA              new end LBA for resize-part (grow or shrink in place)
  --grow-lv              also grow the LV: the one named by --lv, else the
                         only LV
  --grow-to-end          pin the end at the last usable LBA (fill semantics)
  --grow                 append the relocation preview for partition :N
  --keep-partuuid        reuse the source PARTUUID instead of generating a new
                         one
  --label S              filesystem label (omitted = an empty label, not a
                         default)
  --lv NAME              which LV to grow (implies --grow-lv)
  --name S               name for the entry this command creates
  --no-fs                skip the filesystem step of the grown or moved
                         partition
  --overwrite-data       accept overwriting bytes an existing partition holds
  --part-number N        1-based entry slot; it must be empty
  --random               ask the target to generate a new value
  --sector-size N        logical sector size override for raw images (power of
                         two, max 65536 bytes — an implementation limit, not a
                         format requirement)
  --start LBA|end        new start: an LBA, or `end` for pack-to-end
  --table gpt|mbr        table kind: gpt (default) or mbr
  --type-guid GUID|0xNN  GPT: type GUID text, e.g.
                         0fc63daf-8483-4772-8e79-3d69d8477de4; MBR: type byte
                         0xNN
  --yes                  acknowledge a write before it starts

  --yes is required by: resize, move, copy, delete, set, mkfs, resizefs, undo,
  new, add, resize-part

  --flag=VALUE is the same as --flag VALUE, except that a value starting with
  `-` needs the equals form. `--` ends option parsing: every later argument is
  positional, which is how a target or name starting with `-` is passed
exit status:
  0  success
  1  unexpected internal error
  2  usage or command syntax error
  3  policy refusal, or a precondition of the request is not met
  4  operation failed with no persistent modification
  5  partially completed, or recovery is pending (the remedy is printed)
  6  the platform or capability is not supported"#;

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
            format!("{USAGE_SNAPSHOT}\n"),
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
