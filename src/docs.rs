//! 帮助、参考文档与能力矩阵的生成器。
//!
//! 输入只有 [`crate::cli`] 的命令声明与 [`crate::platform::capabilities`] 的设计矩阵：
//! 顶层帮助的命令清单、每条命令的用法行、摘要与选项块都从同一份 `CommandDef` 读出，
//! 解析器的白名单也是这一份。
//!
//! 参考文档与能力矩阵只在测试构建里产出（`docs/`），CI 生成后与工作树比对。

// 本模块只拼装文本：目标始终是 `String`，`fmt::Write` 对这个实现永不失败。
// crate 默认把 `let_underscore_must_use` 设为 deny（见 Cargo.toml），而这里 58 处
// 写入都没有可上报的错误——统一在模块级放行，避免每行各挂一个 allow 把正文淹没。
// 其它模块仍按"忽略必须落在 best_effort_* 函数内"执行。
#![allow(clippy::let_underscore_must_use)]

use std::fmt::Write as _;

use crate::cli::{self, CommandDef, CommandGroup, Confirmation};
use crate::outcome::{
    EXIT_INTERNAL, EXIT_NO_MUTATION, EXIT_OK, EXIT_PARTIAL, EXIT_REFUSED, EXIT_UNSUPPORTED, EXIT_USAGE,
};

/// 折行宽度
const WIDTH: usize = 78;

/// 顶层帮助的示例。示例是命令的组合用法，不是单条声明能推出的；
/// `usage_examples_follow_the_command_syntax` 逐行核对命令名与分区选择器。
pub(crate) const EXAMPLES: &[&str] = &[
    "diskedit info /dev/sdb",
    "diskedit plan /dev/sdb:2 --grow",
    "diskedit resize /dev/sdb:1 +2G",
    "diskedit resize disk.img grow --allow-move --chunk 4 --yes",
    "diskedit undo /dev/sdb --yes",
];

/// 顶层帮助里不归属任何一条命令的分派说明
const TITLE: &str = "diskedit — disk / image editor";
const USAGE_LINES: &[&str] = &[
    "diskedit <COMMAND> [ARGS]                details for one command: diskedit help <CMD>",
    "diskedit --version",
];
const TARGET_NOTE: &[&str] = &[
    "target: image path or block device; :N = partition number (1-based). :last = the last",
    "        partition a command can act on. On MBR an extended container does not count",
    "        as one; a target without a partition table has no :last",
];
const FLAG_NOTE: &[&str] = &[
    "--flag=VALUE is the same as --flag VALUE, except that a value starting with",
    "`-` needs the equals form. `--` ends option parsing: every later argument is",
    "positional, which is how a target or name starting with `-` is passed",
];
const EXIT_LEGEND: &[(u8, &str)] = &[
    (EXIT_OK, "success"),
    (EXIT_INTERNAL, "unexpected internal error"),
    (EXIT_USAGE, "usage or command syntax error"),
    (EXIT_REFUSED, "policy refusal, or a precondition of the request is not met"),
    (EXIT_NO_MUTATION, "operation failed with no persistent modification"),
    (EXIT_PARTIAL, "partially completed, or recovery is pending (the remedy is printed)"),
    (EXIT_UNSUPPORTED, "the platform or capability is not supported"),
];

/// 顶层帮助。用法错误与主动请求帮助读同一个函数，只差出口
pub(crate) fn usage_text() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{TITLE}");
    let _ = writeln!(out);
    let _ = writeln!(out, "usage:");
    for line in USAGE_LINES {
        let _ = writeln!(out, "  {line}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "examples:");
    for line in EXAMPLES {
        let _ = writeln!(out, "  {line}");
    }
    for (group, heading) in [
        (CommandGroup::Everyday, "everyday:"),
        (CommandGroup::LowLevel, "low-level (one step per run; long jobs stay resumable):"),
    ] {
        let _ = writeln!(out);
        let _ = writeln!(out, "{heading}");
        for c in cli::COMMANDS.iter().filter(|c| c.group == group) {
            for line in c.syntax {
                let _ = writeln!(out, "  {} {}", c.name, line);
            }
            push_wrapped(&mut out, c.summary, 6, 6, WIDTH);
        }
    }
    let _ = writeln!(out);
    for line in TARGET_NOTE {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "options (availability: help <CMD>):");
    let rows: Vec<(String, &'static str)> =
        cli::FLAGS.iter().map(|f| (flag_synopsis(f.name), f.desc)).collect();
    let column = rows.iter().map(|(s, _)| s.chars().count()).max().unwrap_or(0) + 2;
    for (synopsis, desc) in rows {
        let pad = " ".repeat(column.saturating_sub(synopsis.chars().count()));
        let lines = wrap(desc, WIDTH.saturating_sub(2 + column).max(20));
        for (i, line) in lines.into_iter().enumerate() {
            if i == 0 {
                let _ = writeln!(out, "  {synopsis}{pad}{line}");
            } else {
                let _ = writeln!(out, "{}{line}", " ".repeat(2 + column));
            }
        }
    }
    let required: Vec<&str> = cli::COMMANDS
        .iter()
        .filter(|c| c.confirmation == Confirmation::Required)
        .map(|c| c.name)
        .collect();
    let _ = writeln!(out);
    push_wrapped(
        &mut out,
        &format!("--yes is required by: {}", required.join(", ")),
        2,
        2,
        WIDTH,
    );
    let _ = writeln!(out);
    for line in FLAG_NOTE {
        let _ = writeln!(out, "  {line}");
    }
    let _ = writeln!(out, "exit status:");
    for (code, desc) in EXIT_LEGEND {
        let _ = writeln!(out, "  {code}  {desc}");
    }
    trim_trailing_newlines(out)
}

/// 单命令详助。用法行、摘要与选项块由声明生成，正文原样取自 `details`
pub(crate) fn help_text(name: &str) -> Option<String> {
    let c = cli::find(name)?;
    let mut out = String::new();
    for line in c.syntax {
        let _ = writeln!(out, "diskedit {} {}", c.name, line);
    }
    let _ = writeln!(out);
    push_wrapped(&mut out, c.summary, 2, 2, WIDTH);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", c.details);
    let _ = writeln!(out);
    push_options(&mut out, c);
    Some(trim_trailing_newlines(out))
}

/// 命令页里列出的选项：命令自己声明的那些。全局旗标只在顶层帮助的选项表里列一次，
/// 无差别地印在每条命令下面只会盖住命令真正的差异
fn option_names(c: &CommandDef) -> Vec<&'static str> {
    c.options.iter().map(|o| o.name).collect()
}

/// 旗标在帮助里的写法：带值的补占位符，布尔旗标只有名字
fn flag_synopsis(name: &str) -> String {
    match cli::flag_spec(name) {
        Some(spec) if spec.domain.takes_value() => {
            format!("{} {}", spec.name, spec.domain.placeholder())
        }
        _ => name.to_string(),
    }
}

/// 选项说明：命令声明的补充优先，否则用旗标注册表的同一条
fn option_desc(c: &CommandDef, name: &str) -> String {
    let declared = c.options.iter().find(|o| o.name == name).and_then(|o| o.desc);
    let base = match declared {
        Some(desc) => desc.to_string(),
        None => cli::flag_spec(name).map(|f| f.desc.to_string()).unwrap_or_default(),
    };
    if name == "--yes" && c.confirmation == Confirmation::Required {
        format!("{base} (required by this command)")
    } else {
        base
    }
}

fn push_options(out: &mut String, c: &CommandDef) {
    let names = option_names(c);
    if names.is_empty() {
        let _ = writeln!(out, "options: none");
        return;
    }
    let synopses: Vec<String> = names.iter().map(|n| flag_synopsis(n)).collect();
    let column = synopses.iter().map(|s| s.chars().count()).max().unwrap_or(0) + 2;
    let _ = writeln!(out, "options:");
    for (name, synopsis) in names.iter().zip(synopses) {
        let desc = option_desc(c, name);
        let pad = " ".repeat(column.saturating_sub(synopsis.chars().count()));
        let lines = wrap(&desc, WIDTH.saturating_sub(4 + column).max(20));
        for (i, line) in lines.into_iter().enumerate() {
            if i == 0 {
                let _ = writeln!(out, "    {synopsis}{pad}{line}");
            } else {
                let _ = writeln!(out, "{}{line}", " ".repeat(4 + column));
            }
        }
    }
}

/// 贪心折行：按空白切词，单行不超过 `width` 列
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// 首行缩进 `indent`，后续行缩进 `hanging`
fn push_wrapped(out: &mut String, text: &str, indent: usize, hanging: usize, width: usize) {
    let width = width.saturating_sub(indent).max(1);
    for (i, line) in wrap(text, width).into_iter().enumerate() {
        let pad = if i == 0 { indent } else { hanging };
        let _ = writeln!(out, "{:pad$}{line}", "");
    }
}

fn trim_trailing_newlines(mut text: String) -> String {
    while text.ends_with('\n') {
        text.pop();
    }
    text
}

/// 仓库内的生成物：只在测试构建里产出，CI 生成后与工作树比对
#[cfg(test)]
mod published {
    use super::*;
    use crate::domain::MediaKind;
    use crate::platform::capabilities::{CapabilityId, CapabilitySnapshot, HostPlatform};

    const REFERENCE: &str = "docs/reference.md";
    const MATRIX: &str = "docs/capability-matrix.md";

    /// 六个平台 × 载体形态组合；矩阵对每一列都给出显式结论
    const SHAPES: [(HostPlatform, MediaKind); 6] = [
        (HostPlatform::Linux, MediaKind::Block),
        (HostPlatform::Linux, MediaKind::Image),
        (HostPlatform::Windows, MediaKind::Block),
        (HostPlatform::Windows, MediaKind::Image),
        (HostPlatform::Other, MediaKind::Block),
        (HostPlatform::Other, MediaKind::Image),
    ];

    fn shape_name(platform: HostPlatform, media: MediaKind) -> &'static str {
        match (platform, media) {
            (HostPlatform::Linux, MediaKind::Block) => "linux / block",
            (HostPlatform::Linux, MediaKind::Image) => "linux / image",
            (HostPlatform::Windows, MediaKind::Block) => "windows / block",
            (HostPlatform::Windows, MediaKind::Image) => "windows / image",
            (HostPlatform::Other, MediaKind::Block) => "other / block",
            (HostPlatform::Other, MediaKind::Image) => "other / image",
        }
    }

    fn mode_name(mode: cli::TargetMode) -> &'static str {
        match mode {
            cli::TargetMode::ReadOnly => "read-only",
            cli::TargetMode::WriteNoJournal => "writes, no undo journal",
            cli::TargetMode::WriteJournal => "writes, undo journal",
        }
    }

    fn join_or_none(items: impl Iterator<Item = String>) -> String {
        let joined = items.collect::<Vec<_>>().join(", ");
        if joined.is_empty() {
            "none".to_string()
        } else {
            joined
        }
    }

    fn render_reference() -> String {
        let mut out = String::new();
        out.push_str("# diskedit command reference\n\n");
        out.push_str("Generated from the command schema in `src/cli.rs` by `src/docs.rs`.\n");
        out.push_str(
            "Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit \
             docs_are_generated_in_sync`;\n",
        );
        out.push_str("CI regenerates it and fails when the working tree differs. Do not edit by hand.\n\n");
        out.push_str("## Commands\n");
        for c in cli::COMMANDS {
            let _ = writeln!(out, "\n### `{}`\n", c.name);
            let _ = writeln!(out, "{}\n", c.summary);
            let _ = writeln!(out, "```text");
            for line in c.syntax {
                let _ = writeln!(out, "diskedit {} {}", c.name, line);
            }
            let _ = writeln!(out, "```\n");
            let _ = writeln!(out, "| property | value |");
            let _ = writeln!(out, "|---|---|");
            let _ = writeln!(
                out,
                "| positionals | {} |",
                join_or_none(c.positionals.iter().map(|p| format!("`{}`", p.name)))
            );
            let _ = writeln!(
                out,
                "| options | {} |",
                join_or_none(option_names(c).into_iter().map(|n| format!("`{n}`")))
            );
            let _ = writeln!(
                out,
                "| confirmation | {} |",
                match c.confirmation {
                    Confirmation::Required => "required (`--yes`)",
                    Confirmation::None => "none",
                }
            );
            let _ = writeln!(out, "| journal | {} |", mode_name(c.mode));
            let _ = writeln!(
                out,
                "| capabilities | {} |",
                join_or_none(c.capabilities.iter().map(|cap| capability_category(*cap).to_string()))
            );
            let _ = writeln!(out, "\n```text");
            let _ = writeln!(out, "{}", c.details);
            let _ = writeln!(out, "```");
        }
        out.push_str("\n## Global options\n\n");
        out.push_str("| flag | value | description |\n|---|---|---|\n");
        for f in cli::FLAGS {
            let value = if f.domain.takes_value() {
                format!("`{}`", f.domain.placeholder())
            } else {
                "—".to_string()
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | {} |",
                f.name,
                table_cell(&value),
                table_cell(f.desc)
            );
        }
        out.push_str("\n## Exit status\n\n| code | meaning |\n|---|---|\n");
        for (code, desc) in EXIT_LEGEND {
            let _ = writeln!(out, "| {code} | {desc} |");
        }
        trim_trailing_newlines(out)
    }

    /// 表格单元格里的 `|` 会拆列：值域占位符（`LBA|end`）与旗标说明（`mib|none`）
    /// 都含竖线，写进 Markdown 表前必须转义
    fn table_cell(text: &str) -> String {
        text.replace('|', "\\|")
    }

    fn capability_category(cap: cli::Capability) -> &'static str {
        match cap {
            cli::Capability::ReadOnly => "readonly",
            cli::Capability::TargetData => "target data",
            cli::Capability::CompanionFile => "companion file",
            cli::Capability::ExternalTool => "external tool",
            cli::Capability::DeviceState => "device state",
        }
    }

    fn render_matrix() -> String {
        let mut out = String::new();
        out.push_str("# diskedit capability matrix\n\n");
        out.push_str(
            "Generated from `CommandDef.capabilities` and `CapabilitySnapshot::design(platform, \
             media)` by `src/docs.rs`.\n",
        );
        out.push_str(
            "Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit \
             docs_are_generated_in_sync`;\n",
        );
        out.push_str("CI regenerates it and fails when the working tree differs. Do not edit by hand.\n\n");

        out.push_str("## Capability states\n\n| capability |");
        for (platform, media) in SHAPES {
            let _ = write!(out, " {} |", shape_name(platform, media));
        }
        let _ = writeln!(out, "\n|---|{}", "---|".repeat(SHAPES.len()));
        for id in CapabilityId::ALL {
            let _ = write!(out, "| {} |", id.as_str());
            for (platform, media) in SHAPES {
                let snapshot = CapabilitySnapshot::design(platform, media);
                let _ = write!(out, " {} |", snapshot.state(id).as_str());
            }
            let _ = writeln!(out);
        }

        out.push_str("\n## Commands\n\n| command |");
        for (platform, media) in SHAPES {
            let _ = write!(out, " {} |", shape_name(platform, media));
        }
        let _ = writeln!(out, "\n|---|{}", "---|".repeat(SHAPES.len()));
        for c in cli::COMMANDS {
            let needs = crate::planner::needs_for(c.capabilities);
            let _ = write!(out, "| `{}` |", c.name);
            for (platform, media) in SHAPES {
                let snapshot = CapabilitySnapshot::design(platform, media);
                let cell = match snapshot.first_gap(&needs) {
                    None => "admitted".to_string(),
                    Some((id, state)) => {
                        format!("blocked: {} ({})", id.as_str(), state.as_str())
                    }
                };
                let _ = write!(out, " {cell} |");
            }
            let _ = writeln!(out);
        }

        out.push_str("\n## Runtime gate\n\n");
        out.push_str(
            "A cell records whether that platform/media pair has the primitives the command needs.\n\
             The runtime gate reads the same matrix, restricted to the media the host can actually open\n\
             (`HostPlatform::openable_media`): Linux opens block devices and image files; other hosts open\n\
             image files only. A read-only command therefore runs on a non-Linux image file, while any\n\
             command needing a write-side or evidence-side primitive answers exit 6.\n",
        );
        trim_trailing_newlines(out)
    }

    /// 生成物与工作树逐字一致。设置 `DISKEDIT_WRITE_DOCS` 时先写盘，再比对
    #[test]
    fn docs_are_generated_in_sync() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for (rel, text) in [(REFERENCE, render_reference()), (MATRIX, render_matrix())] {
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("cyl"), "{rel}: cylinder alignment leaked into the generated text");
            assert!(!lower.contains("chs"), "{rel}: CHS geometry leaked into the generated text");

            let path = root.join(rel);
            let before = std::fs::read_to_string(&path).ok();
            if std::env::var_os("DISKEDIT_WRITE_DOCS").is_some() {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).expect("create the docs directory");
                }
                std::fs::write(&path, &text).expect("write the generated document");
                // 写模式下也要有信号：内容变了就红一次，提醒把生成物真正提交。
                // 写完再和自己比永远绿，等于把"生成物没跟上"这件事藏起来
                assert_eq!(
                    before.as_deref(), Some(text.as_str()),
                    "{rel} was regenerated — commit the updated file, then re-run without DISKEDIT_WRITE_DOCS"
                );
            }
            let on_disk = before.unwrap_or_else(|| {
                panic!(
                    "{}: missing\nregenerate with DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit \
                     docs_are_generated_in_sync",
                    path.display()
                )
            });
            assert_eq!(
                on_disk, text,
                "{rel} is out of date: the schema changed; regenerate with DISKEDIT_WRITE_DOCS=1"
            );
        }
    }
}
