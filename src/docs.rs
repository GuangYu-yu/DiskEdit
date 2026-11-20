//! 帮助、参考文档与能力矩阵的生成器。
//!
//! 输入只有 [`crate::cli`] 的命令声明与 [`crate::platform::capabilities`] 的设计矩阵：
//! 顶层帮助的命令清单、每条命令的用法行、摘要与选项块都从同一份 `CommandDef` 读出，
//! 解析器的白名单也是这一份。
//!
//! 参考文档与能力矩阵只在测试构建里产出（`docs/`），CI 生成后与工作树比对。

// 本模块只拼装文本：目标始终是 `String`，`fmt::Write` 对这个实现永不失败。
// crate 把 `let_underscore_must_use` 设为 deny（见 Cargo.toml），所以写入统一走
// `emit!` / `emit_raw!`：`expect` 只在宏里出现一次，把"不可失败"写成显式断言，
// 而不是丢弃返回值
macro_rules! emit {
    ($out:expr $(, $arg:expr)* $(,)?) => {
        writeln!($out $(, $arg)*).expect("writing to a String cannot fail")
    };
}

#[cfg(test)]
macro_rules! emit_raw {
    ($out:expr $(, $arg:expr)* $(,)?) => {
        write!($out $(, $arg)*).expect("writing to a String cannot fail")
    };
}

use std::fmt::Write as _;

use crate::cli::{self, CommandDef, CommandGroup, Confirmation};
use crate::outcome::{
    EXIT_INTERNAL, EXIT_NO_MUTATION, EXIT_OK, EXIT_PARTIAL, EXIT_REFUSED, EXIT_UNSUPPORTED, EXIT_USAGE,
};

const WIDTH: usize = 78;

/// 顶层帮助的示例。示例是命令的组合用法，不是单条声明能推出的；
/// `usage_examples_follow_the_command_syntax` 逐行核对命令名与分区选择器。
pub(crate) const EXAMPLES: &[&str] = &[
    "diskedit info /dev/sdb",
    "diskedit info /dev/sdb:2 --grow",
    "diskedit resize /dev/sdb:1 +2G --yes",
    "diskedit resize disk.img grow --allow-move --chunk 4 --yes",
    "diskedit recover /dev/sdb --yes",
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
    (
        EXIT_NO_MUTATION,
        "this run wrote no target bytes and no record entry was created, renamed, moved away or \
         deleted (the records directory and record.lock do not count)",
    ),
    (
        EXIT_PARTIAL,
        "partial, records changed while the target was not fully written, or applied with follow-up \
         steps pending or a stale kernel view (the remedy is printed)",
    ),
    (EXIT_UNSUPPORTED, "the platform or capability is not supported"),
];

/// 顶层帮助。用法错误与主动请求帮助读同一个函数，只差出口
pub(crate) fn usage_text() -> String {
    let mut out = String::new();
    emit!(out, "{TITLE}");
    emit!(out);
    emit!(out, "usage:");
    for line in USAGE_LINES {
        emit!(out, "  {line}");
    }
    emit!(out);
    emit!(out, "examples:");
    for line in EXAMPLES {
        emit!(out, "  {line}");
    }
    for (group, heading) in [
        (CommandGroup::Everyday, "everyday:"),
        (CommandGroup::LowLevel, "low-level (one step per run; long jobs stay resumable):"),
    ] {
        emit!(out);
        emit!(out, "{heading}");
        for c in cli::COMMANDS.iter().filter(|c| c.group == group) {
            for line in c.syntax_lines() {
                emit!(out, "  {} {}", c.name, line);
            }
            push_wrapped(&mut out, c.summary, 6, 6, WIDTH);
        }
    }
    emit!(out);
    for line in TARGET_NOTE {
        emit!(out, "{line}");
    }
    emit!(out);
    emit!(out, "options (availability: help <CMD>):");
    let rows: Vec<(String, &'static str)> =
        cli::FLAGS.iter().map(|f| (flag_synopsis(f.name), f.desc)).collect();
    let column = rows.iter().map(|(s, _)| s.chars().count()).max().unwrap_or(0) + 2;
    for (synopsis, desc) in rows {
        let pad = " ".repeat(column.saturating_sub(synopsis.chars().count()));
        let lines = wrap(desc, WIDTH.saturating_sub(2 + column).max(20));
        for (i, line) in lines.into_iter().enumerate() {
            if i == 0 {
                emit!(out, "  {synopsis}{pad}{line}");
            } else {
                emit!(out, "{}{line}", " ".repeat(2 + column));
            }
        }
    }
    let required: Vec<&str> = cli::COMMANDS
        .iter()
        .filter(|c| c.confirmation == Confirmation::Required)
        .map(|c| c.name)
        .collect();
    emit!(out);
    push_wrapped(
        &mut out,
        &format!("--yes is required by: {}", required.join(", ")),
        2,
        2,
        WIDTH,
    );
    emit!(out);
    for line in FLAG_NOTE {
        emit!(out, "  {line}");
    }
    emit!(out, "exit status:");
    for (code, desc) in EXIT_LEGEND {
        emit!(out, "  {code}  {desc}");
    }
    trim_trailing_newlines(out)
}

/// 单命令详助。用法行、摘要与选项块由声明生成，正文原样取自 `details`
pub(crate) fn help_text(name: &str) -> Option<String> {
    let c = cli::find(name)?;
    let mut out = String::new();
    for line in c.syntax_lines() {
        emit!(out, "diskedit {} {}", c.name, line);
    }
    emit!(out);
    push_wrapped(&mut out, c.summary, 2, 2, WIDTH);
    emit!(out);
    emit!(out, "{}", c.details);
    emit!(out);
    push_options(&mut out, c);
    Some(trim_trailing_newlines(out))
}

/// 命令页里列出的选项：按形态分组，命令级公共选项并入每组。全局旗标只在顶层帮助的
/// 选项表里列一次，无差别地印在每条命令下面只会盖住命令真正的差异
fn option_groups(c: &CommandDef) -> Vec<(Vec<&'static str>, Vec<&'static str>)> {
    let mut groups: Vec<(Vec<&'static str>, Vec<&'static str>)> = Vec::new();
    for v in c.variants {
        let mut names: Vec<&'static str> = c.common_options.iter().map(|o| o.name).collect();
        for o in v.options {
            if !names.contains(&o.name) {
                names.push(o.name);
            }
        }
        names.sort_unstable();
        match groups.iter_mut().find(|(n, _)| *n == names) {
            Some((_, syntaxes)) => syntaxes.push(v.syntax),
            None => groups.push((names, vec![v.syntax])),
        }
    }
    groups
}

/// 命令声明的全部选项（任一形态或公共选项），按注册顺序去重。只供参考文档的
/// 汇总表使用；帮助按形态分组（见 [`option_groups`]）
#[cfg(test)]
fn option_names(c: &CommandDef) -> Vec<&'static str> {
    let mut names = Vec::new();
    for o in c.common_options.iter().chain(c.variants.iter().flat_map(|v| v.options.iter())) {
        if !names.contains(&o.name) {
            names.push(o.name);
        }
    }
    names
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

/// 选项说明：形态声明的补充优先，其次命令级公共选项，最后用旗标注册表的同一条
fn option_desc(c: &CommandDef, name: &str) -> String {
    let declared = c
        .variants
        .iter()
        .flat_map(|v| v.options.iter())
        .chain(c.common_options.iter())
        .find(|o| o.name == name)
        .and_then(|o| o.desc);
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
    let groups = option_groups(c);
    let multi = c.variants.len() > 1;
    for (names, syntaxes) in groups {
        if multi {
            let forms: Vec<String> = syntaxes
                .iter()
                .map(|s| format!("diskedit {} {s}", c.name))
                .collect();
            emit!(out, "options for `{}`:", forms.join("` / `"));
        } else {
            emit!(out, "options:");
        }
        if names.is_empty() {
            emit!(out, "    none");
            continue;
        }
        let synopses: Vec<String> = names.iter().map(|n| flag_synopsis(n)).collect();
        let column = synopses.iter().map(|s| s.chars().count()).max().unwrap_or(0) + 2;
        for (name, synopsis) in names.iter().zip(synopses) {
            let desc = option_desc(c, name);
            let pad = " ".repeat(column.saturating_sub(synopsis.chars().count()));
            let lines = wrap(&desc, WIDTH.saturating_sub(4 + column).max(20));
            for (i, line) in lines.into_iter().enumerate() {
                if i == 0 {
                    emit!(out, "    {synopsis}{pad}{line}");
                } else {
                    emit!(out, "{}{line}", " ".repeat(4 + column));
                }
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

/// 首行缩进 `indent`，后续行缩进 `hanging`；正文按 `width - indent` 个字符折行
/// （`width` 不大于 `indent` 时按 1 个），因此首行总宽是 `indent + 该宽度`，
/// 后续行总宽是 `hanging + 该宽度`：`hanging > indent` 时超出 `width`，反之短于 `width`
fn push_wrapped(out: &mut String, text: &str, indent: usize, hanging: usize, width: usize) {
    let width = width.saturating_sub(indent).max(1);
    for (i, line) in wrap(text, width).into_iter().enumerate() {
        let pad = if i == 0 { indent } else { hanging };
        emit!(out, "{:pad$}{line}", "");
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
    use crate::outcome::NOTHING_WRITTEN;
    use crate::platform::capabilities::{CapabilityId, CapabilitySnapshot, HostPlatform};

    const REFERENCE_EN: &str = "docs/en/reference.md";
    const MATRIX_EN: &str = "docs/en/capability-matrix.md";
    const REFERENCE_ZH: &str = "docs/zh/reference.md";
    const MATRIX_ZH: &str = "docs/zh/capability-matrix.md";

    /// 中文文案目录：键是渲染时用的英文原文（标题、表格行与带 `{n}` 占位符的模板），
    /// 值是中文。只由测试构建读取，不进入二进制
    const CATALOGUE: &str = include_str!("../docs/zh/catalogue.json");

    /// 六个平台 × 载体形态组合；矩阵对每一列都给出显式结论
    const SHAPES: [(HostPlatform, MediaKind); 6] = [
        (HostPlatform::Linux, MediaKind::Block),
        (HostPlatform::Linux, MediaKind::Image),
        (HostPlatform::Windows, MediaKind::Block),
        (HostPlatform::Windows, MediaKind::Image),
        (HostPlatform::Other, MediaKind::Block),
        (HostPlatform::Other, MediaKind::Image),
    ];

    /// 中文目录：渲染时按英文原文查表，缺项与多余项都由 `docs_are_generated_in_sync`
    /// 报出，因此中文文档与 schema 不存在各自漂移的余地
    struct Catalogue {
        entries: serde_json::Map<String, serde_json::Value>,
        used: std::cell::RefCell<std::collections::BTreeSet<String>>,
        missing: std::cell::RefCell<std::collections::BTreeSet<String>>,
    }

    impl Catalogue {
        fn load() -> Self {
            let entries = match serde_json::from_str(CATALOGUE) {
                Ok(serde_json::Value::Object(map)) => map,
                Ok(_) => panic!("docs/zh/catalogue.json: the top level must be an object"),
                Err(e) => panic!("docs/zh/catalogue.json: {e}"),
            };
            for (key, value) in &entries {
                let text = value.as_str().unwrap_or_else(|| {
                    panic!("docs/zh/catalogue.json: the value of `{key}` is not a string")
                });
                assert!(
                    !text.trim().is_empty(),
                    "docs/zh/catalogue.json: `{key}` has no translation"
                );
            }
            Self { entries, used: Default::default(), missing: Default::default() }
        }

        /// 缺译文时回退英文并记下键，测试据此一次列出全部缺口
        fn t<'a>(&'a self, key: &'a str) -> &'a str {
            match self.entries.get(key).and_then(|v| v.as_str()) {
                Some(text) => {
                    self.used.borrow_mut().insert(key.to_string());
                    text
                }
                None => {
                    self.missing.borrow_mut().insert(key.to_string());
                    key
                }
            }
        }

        fn fill(&self, key: &str, parts: &[&str]) -> String {
            fill_template(self.t(key), parts)
        }

        fn unused(&self) -> Vec<String> {
            let used = self.used.borrow();
            self.entries.keys().filter(|k| !used.contains(*k)).cloned().collect()
        }
    }

    fn fill_template(template: &str, parts: &[&str]) -> String {
        let mut text = template.to_string();
        for (index, part) in parts.iter().enumerate() {
            text = text.replace(&format!("{{{index}}}"), part);
        }
        text
    }

    fn tr<'a>(cat: Option<&'a Catalogue>, en: &'a str) -> &'a str {
        match cat {
            Some(cat) => cat.t(en),
            None => en,
        }
    }

    fn fill(cat: Option<&Catalogue>, template: &str, parts: &[&str]) -> String {
        match cat {
            Some(cat) => cat.fill(template, parts),
            None => fill_template(template, parts),
        }
    }

    fn platform_name(platform: HostPlatform) -> &'static str {
        match platform {
            HostPlatform::Linux => "linux",
            HostPlatform::Windows => "windows",
            HostPlatform::Other => "other",
        }
    }

    fn media_name(media: MediaKind) -> &'static str {
        match media {
            MediaKind::Block => "block",
            MediaKind::Image => "image",
        }
    }

    fn shape_name(cat: Option<&Catalogue>, platform: HostPlatform, media: MediaKind) -> String {
        fill(cat, "{0} / {1}", &[tr(cat, platform_name(platform)), tr(cat, media_name(media))])
    }

    fn mode_name(mode: cli::TargetMode) -> &'static str {
        match mode {
            cli::TargetMode::ReadOnly => "read-only",
            cli::TargetMode::WriteNoJournal => "writes, no recovery record",
            cli::TargetMode::WriteJournal => "writes, recovery record",
        }
    }

    fn join_or_none(cat: Option<&Catalogue>, items: impl Iterator<Item = String>) -> String {
        let joined = items.collect::<Vec<_>>().join(", ");
        if joined.is_empty() {
            tr(cat, "none").to_string()
        } else {
            joined
        }
    }

    fn render_reference(cat: Option<&Catalogue>) -> String {
        let mut out = String::new();
        out.push_str(tr(cat, "# diskedit command reference\n\n"));
        out.push_str(tr(
            cat,
            "Generated from the command schema in `src/cli.rs` by `src/docs.rs`.\n",
        ));
        out.push_str(tr(
            cat,
            "Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit \
             docs_are_generated_in_sync`;\n",
        ));
        out.push_str(tr(
            cat,
            "CI regenerates it and fails when the working tree differs. Do not edit by hand.\n\n",
        ));
        out.push_str(tr(cat, "## Commands\n"));
        for c in cli::COMMANDS {
            emit!(out, "\n### `{}`\n", c.name);
            emit!(out, "{}\n", tr(cat, c.summary));
            emit!(out, "```text");
            for line in c.syntax_lines() {
                emit!(out, "diskedit {} {}", c.name, line);
            }
            emit!(out, "```\n");
            emit!(out, "{}", tr(cat, "| property | value |"));
            emit!(out, "|---|---|");
            let forms: Vec<String> = c
                .variants
                .iter()
                .map(|v| {
                    join_or_none(
                        cat,
                        v.positionals.iter().map(|p| {
                            if p.optional {
                                fill(cat, "`{0}` (optional)", &[p.name])
                            } else {
                                format!("`{}`", p.name)
                            }
                        }),
                    )
                })
                .collect();
            emit!(out, "{}", fill(cat, "| positionals | {0} |", &[&forms.join(" / ")]));
            emit!(
                out,
                "{}",
                fill(
                    cat,
                    "| options | {0} |",
                    &[&join_or_none(
                        cat,
                        option_names(c).into_iter().map(|n| format!("`{n}`"))
                    )]
                )
            );
            emit!(
                out,
                "{}",
                fill(
                    cat,
                    "| confirmation | {0} |",
                    &[tr(
                        cat,
                        match c.confirmation {
                            Confirmation::Required => "required (`--yes`)",
                            Confirmation::None => "none",
                        }
                    )]
                )
            );
            emit!(
                out,
                "{}",
                fill(cat, "| journal | {0} |", &[tr(cat, mode_name(c.mode))])
            );
            emit!(
                out,
                "{}",
                fill(
                    cat,
                    "| capabilities | {0} |",
                    &[&join_or_none(
                        cat,
                        c.capabilities
                            .iter()
                            .map(|cap| tr(cat, capability_category(*cap)).to_string())
                    )]
                )
            );
            emit!(out, "\n```text");
            emit!(out, "{}", tr(cat, c.details));
            emit!(out, "```");
        }
        out.push_str(tr(cat, "\n## Global options\n\n"));
        out.push_str(tr(cat, "| flag | value | description |\n|---|---|---|\n"));
        for f in cli::FLAGS {
            let value = if f.domain.takes_value() {
                format!("`{}`", f.domain.placeholder())
            } else {
                "—".to_string()
            };
            emit!(
                out,
                "| `{}` | {} | {} |",
                f.name,
                table_cell(&value),
                table_cell(tr(cat, f.desc))
            );
        }
        out.push_str(tr(cat, "\n## Exit status\n\n"));
        out.push_str(tr(cat, "| code | meaning |\n|---|---|\n"));
        for (code, desc) in EXIT_LEGEND {
            emit!(out, "| {code} | {} |", tr(cat, desc));
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

    fn render_matrix(cat: Option<&Catalogue>) -> String {
        let mut out = String::new();
        out.push_str(tr(cat, "# diskedit capability matrix\n\n"));
        out.push_str(tr(
            cat,
            "Generated from `CommandDef.capabilities` and `CapabilitySnapshot::design(platform, \
             media)` by `src/docs.rs`.\n",
        ));
        out.push_str(tr(
            cat,
            "Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit \
             docs_are_generated_in_sync`;\n",
        ));
        out.push_str(tr(
            cat,
            "CI regenerates it and fails when the working tree differs. Do not edit by hand.\n\n",
        ));

        out.push_str(tr(cat, "## Capability states\n\n"));
        emit_raw!(out, "| {} |", tr(cat, "capability"));
        for (platform, media) in SHAPES {
            emit_raw!(out, " {} |", shape_name(cat, platform, media));
        }
        emit!(out, "\n|---|{}", "---|".repeat(SHAPES.len()));
        for id in CapabilityId::ALL {
            emit_raw!(out, "| {} |", id.as_str());
            for (platform, media) in SHAPES {
                let snapshot = CapabilitySnapshot::design(platform, media);
                emit_raw!(out, " {} |", tr(cat, snapshot.state(id).as_str()));
            }
            emit!(out);
        }

        out.push_str(tr(cat, "\n## Commands\n\n"));
        emit_raw!(out, "| {} |", tr(cat, "command"));
        for (platform, media) in SHAPES {
            emit_raw!(out, " {} |", shape_name(cat, platform, media));
        }
        emit!(out, "\n|---|{}", "---|".repeat(SHAPES.len()));
        for c in cli::COMMANDS {
            let needs = crate::planner::needs_for(c.capabilities);
            emit_raw!(out, "| `{}` |", c.name);
            for (platform, media) in SHAPES {
                let snapshot = CapabilitySnapshot::design(platform, media);
                let cell = match snapshot.first_gap(&needs) {
                    None => tr(cat, "admitted").to_string(),
                    Some((id, state)) => fill(
                        cat,
                        "blocked: {0} ({1})",
                        &[id.as_str(), tr(cat, state.as_str())],
                    ),
                };
                emit_raw!(out, " {cell} |");
            }
            emit!(out);
        }

        out.push_str(tr(cat, "\n## Runtime gate\n\n"));
        // 只列本平台真正能打开的形态，且平台固定在 Linux：生成物在两个构建目标上
        // 必须逐字一致，`HostPlatform::current()` 会让同一份文档随宿主变化
        let gate_shapes = HostPlatform::Linux.openable_media();
        let listed = gate_shapes
            .iter()
            .map(|media| shape_name(cat, HostPlatform::Linux, *media))
            .collect::<Vec<_>>()
            .join(", ");
        emit!(
            out,
            "{}",
            fill(
                cat,
                "The gate evaluates only the target shapes the build can open. This table lists the \
                 {0} target shapes it evaluates on Linux (`HostPlatform::openable_media`): {1}. \
                 Other shapes do not enter the gate's decision — `cmd::capability_gap` iterates only \
                 the openable set.",
                &[&gate_shapes.len().to_string(), &listed]
            )
        );
        out.push_str(tr(
            cat,
            "The table is generated from the Linux capability set; a non-Linux build's openable set \
             is decided at run time by `HostPlatform::openable_media`, and this document does not \
             copy it.\n\n",
        ));
        emit_raw!(out, "| {} |", tr(cat, "command"));
        for media in gate_shapes {
            emit_raw!(out, " {} |", shape_name(cat, HostPlatform::Linux, *media));
        }
        emit!(out, "\n|---|{}", "---|".repeat(gate_shapes.len()));
        for c in cli::COMMANDS {
            let needs = crate::planner::needs_for(c.capabilities);
            emit_raw!(out, "| `{}` |", c.name);
            for media in gate_shapes {
                let snapshot = CapabilitySnapshot::design(HostPlatform::Linux, *media);
                let cell = match snapshot.first_gap(&needs) {
                    None => tr(cat, "admitted").to_string(),
                    Some((id, state)) => fill(
                        cat,
                        "blocked: {0} ({1})",
                        &[id.as_str(), tr(cat, state.as_str())],
                    ),
                };
                emit_raw!(out, " {cell} |");
            }
            emit!(out);
        }
        emit!(
            out,
            "\n{}",
            fill(
                cat,
                "The process-level exit prints the cause sentence of a failure. Exit {0} and exit \
                 {1} are followed by a second line, `{2}`; exit {3} prints `unsupported: <cause>` \
                 and appends no such line.",
                &[
                    &EXIT_REFUSED.to_string(),
                    &EXIT_NO_MUTATION.to_string(),
                    NOTHING_WRITTEN,
                    &EXIT_UNSUPPORTED.to_string(),
                ]
            )
        );
        trim_trailing_newlines(out)
    }

    /// `internals.md` 只留顺序陈述：一句话里同时出现该退出码与命令名，等于把矩阵的
    /// 逐命令结论又抄了一份。命令名取自命令表，不另写清单；两个语种各查一遍
    fn assert_no_per_command_gate_claims(root: &std::path::Path) {
        const DOCS: &[(&str, &str)] = &[
            ("docs/en/internals.md", "exit code 6"),
            ("docs/zh/internals.md", "退出码 6"),
        ];
        for (doc, claim) in DOCS {
            let path = root.join(doc);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for sentence in text.split(['.', '。', '\n']) {
                if !sentence.contains(claim) {
                    continue;
                }
                if let Some(command) = cli::COMMANDS.iter().find(|c| sentence.contains(c.name)) {
                    panic!(
                        "{}: one sentence names both `{claim}` and the command `{}` — the \
                         per-command answer belongs to docs/en/capability-matrix.md",
                        path.display(),
                        command.name
                    );
                }
            }
        }
    }

    /// 生成物与工作树逐字一致；中文版还要对账翻译目录——缺译与多余项都算失败。
    /// 设置 `DISKEDIT_WRITE_DOCS` 时先写盘，再比对
    #[test]
    fn docs_are_generated_in_sync() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_no_per_command_gate_claims(root);

        let cat = Catalogue::load();
        let zh_reference = render_reference(Some(&cat));
        let zh_matrix = render_matrix(Some(&cat));
        let missing = cat.missing.borrow();
        assert!(
            missing.is_empty(),
            "docs/zh/catalogue.json lacks translations for:\n{}",
            missing.iter().cloned().collect::<Vec<_>>().join("\n")
        );
        drop(missing);
        let unused = cat.unused();
        assert!(
            unused.is_empty(),
            "docs/zh/catalogue.json has entries the render no longer uses:\n{}",
            unused.join("\n")
        );

        for (rel, text) in [
            (REFERENCE_EN, render_reference(None)),
            (MATRIX_EN, render_matrix(None)),
            (REFERENCE_ZH, zh_reference),
            (MATRIX_ZH, zh_matrix),
        ] {
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
