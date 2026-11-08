//! §8 阶段 5 的验收入口：坏参数在打开目标之前就被拒掉，一律退 2 且原因在 stderr。
use std::process::Command;

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        ),
    )
}

fn rejected(args: &[&str], needle: &str) {
    let (code, text) = run(args);
    assert_eq!(code, 2, "{args:?} must be a usage error, got {code}: {text}");
    assert!(text.contains(needle), "{args:?}: expected {needle:?} in the reason: {text}");
}

#[test]
fn unknown_command_and_undeclared_options_are_refused() {
    rejected(&["frobnicate"], "usage");
    rejected(&["info", "img.bin", "--bogus"], "unknown flag");
    rejected(&["info", "img.bin", "--yes"], "is not a valid option");
    rejected(&["resize", "img.bin:1", "+2G", "--align", "mib"], "is not a valid option");
    rejected(&["help", "frobnicate"], "usage");
}

/// `--chunk-size` 不在完整注册表内；`resize --start/--end`、`resize-part --start`、
/// `resize-part --no-fs`、`add --end` 明确列入「未登记（收到即 2）」。未声明选项 → 2。
#[test]
fn unregistered_flags_are_refused() {
    rejected(
        &["move", "img.bin:1", "--chunk-size", "4"],
        "unknown flag `--chunk-size`",
    );
    rejected(
        &["resize", "img.bin:1", "1G", "--start", "5"],
        "not a valid option for `resize`",
    );
    rejected(
        &["resize", "img.bin:1", "1G", "--end", "9"],
        "not a valid option for `resize`",
    );
    rejected(
        &["resize-part", "img.bin:1", "--start", "5", "--end", "9"],
        "not a valid option for `resize-part`",
    );
    rejected(
        &["resize-part", "img.bin:1", "--end", "9", "--no-fs"],
        "not a valid option for `resize-part`",
    );
    rejected(
        &["add", "img.bin", "1M", "--end", "9"],
        "not a valid option for `add`",
    );
}

#[test]
fn missing_values_are_refused() {
    for (cmd, flag) in [
        ("info", "--sector-size"),
        ("move", "--align"),
        ("move", "--chunk"),
        ("move", "--start"),
        ("resize-part", "--end"),
        ("add", "--type-guid"),
        ("add", "--name"),
        ("mkfs", "--label"),
        ("new", "--table"),
        ("resize", "--lv"),
    ] {
        rejected(&[cmd, flag], "requires a value");
    }
}

#[test]
fn invalid_value_domains_are_refused() {
    for (args, needle) in [
        (&["info", "img.bin", "--sector-size", "abc"][..], "bytes, e.g. 4096"),
        (&["info", "img.bin", "--sector-size", "513"][..], "power of two"),
        (&["info", "img.bin", "--sector-size", "131072"][..], "implementation limit"),
        (&["move", "img.bin:1", "--align", "cyl"][..], "invalid alignment"),
        (&["move", "img.bin:1", "--chunk", "0"][..], "1..=64"),
        (&["move", "img.bin:1", "--chunk", "65"][..], "1..=64"),
        (&["move", "img.bin:1", "--start", "abc"][..], "an LBA or `end`"),
        (&["resize-part", "img.bin:1", "--end", "abc"][..], "an LBA"),
        (&["add", "img.bin", "1M", "--type-guid", "nope"][..], "GUID"),
        (&["add", "img.bin", "1M", "--part-number", "x"][..], "partition number"),
        (&["new", "img.bin", "--table", "bsd"][..], "gpt|mbr"),
        (&["resize", "img.bin:1", "10XB"][..], "bad SIZE"),
        (&["resizefs", "/mnt/point", "10XB"][..], "bad BYTES"),
    ] {
        rejected(args, needle);
    }
}

#[test]
fn repeated_and_malformed_flags_are_refused() {
    rejected(&["add", "img.bin", "--name", "a", "--name", "b"], "more than once");
    rejected(&["move", "img.bin:1", "--start", "2048", "--start=4096"], "more than once");
    rejected(&["delete", "img.bin:1", "--yes=1"], "takes no value");
    rejected(&["--version", "img.bin"], "`--version` takes no");
    // uuid 的显式值与 --random 互斥：argv 已能判定，打开目标之前就该退 2
    rejected(&["set", "img.bin:1", "uuid", "1234", "--random", "--yes"], "not both");
}

#[test]
fn extra_positionals_and_relative_sizes_are_refused() {
    rejected(&["info", "img.bin", "extra"], "unexpected extra argument");
    rejected(&["delete", "img.bin:1", "extra"], "unexpected extra argument");
    rejected(&["info", "-10G"], "unknown flag");
    rejected(&["inspect", "img.bin", "-2G"], "unknown flag");
}
