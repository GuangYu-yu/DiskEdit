//! 文档生成器：参考、能力矩阵、退出码与程序清单的唯一来源。
//!
//! 生成物入库，`tests/generated_docs.rs` 逐字比对；手改生成物或用 `DISKEDIT_WRITE_DOCS=1`
//! 重新生成后未提交，都会让门禁变红。命令面读 [`declare`](crate::app::cli::declare)，
//! 文件系统能力读 `diskedit_core::fs::catalog`，退出码读
//! [`exit`](crate::app::report::exit)；本文件只负责排版，不新增事实。

use crate::app::cli::declare::{self, Effect, Group, TargetSuffix};
use crate::app::report::exit;
use diskedit_core::fs::{ContentKind, Program, Support, Verb, catalog};

/// 生成物：相对仓库根的路径与完整内容。
pub fn files() -> Vec<(&'static str, String)> {
    vec![
        ("docs/en/reference.md", reference()),
        ("docs/en/capability-matrix.md", capability_matrix()),
        ("docs/en/exit-codes.md", exit_codes()),
    ]
}

/// 双语逐段对账表的入库文件名（相对仓库根）。
///
/// 双语目录（`docs/en`、`docs/zh`）只放 Markdown；对账契约单独放 `docs/` 下，
/// 由本函数生成、`tests/generated_docs.rs` 逐字比对——任何一侧增删段都会红。
pub const I18N_CATALOGUE: &str = "docs/catalogue.json";

/// 一段：标题行与它下面的段落（按空行分段，每段取首行作识读标签）。
struct DocSection<'a> {
    heading: &'a str,
    paragraphs: Vec<&'a str>,
}

fn sections_of(text: &str) -> Vec<DocSection<'_>> {
    let mut out: Vec<DocSection<'_>> = Vec::new();
    let mut paragraph_open = false;
    for line in text.lines() {
        if line.starts_with('#') {
            paragraph_open = false;
            out.push(DocSection {
                heading: line,
                paragraphs: Vec::new(),
            });
            continue;
        }
        let empty = line.trim().is_empty();
        if let Some(section) = out.last_mut() {
            if empty {
                paragraph_open = false;
            } else if !paragraph_open {
                section.paragraphs.push(line);
                paragraph_open = true;
            }
        }
    }
    out
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// 生成双语对账表：英文生成物与中文译本的每一段一一对应，段序与段落数一致。
///
/// `pairs` 是 `(英文路径, 英文正文, 中文路径, 中文正文)`。不对齐即 panic——
/// 生成器本身就是守卫，失败消息指出缺哪一段，写盘不会发生。
pub fn i18n_catalogue(pairs: &[(&str, &str, &str, &str)]) -> String {
    let mut out = String::from(
        "{\n  \"generated_by\": \"docsgen\",\n  \"note\": \"Bilingual section ledger: every English section needs a Chinese section in the same order with the same paragraph count. Generated file; do not edit by hand.\",\n  \"regenerate\": \"DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs\",\n  \"documents\": [\n",
    );
    for (index, (en_path, en_text, zh_path, zh_text)) in pairs.iter().enumerate() {
        let en_sections = sections_of(en_text);
        let zh_sections = sections_of(zh_text);
        assert_eq!(
            en_sections.len(),
            zh_sections.len(),
            "{zh_path} 的段数与 {en_path} 不一致：英文 {} 段、中文 {} 段",
            en_sections.len(),
            zh_sections.len()
        );
        out.push_str(&format!(
            "    {{\n      \"en\": \"{}\",\n      \"zh\": \"{}\",\n      \"sections\": [\n",
            json_escape(en_path),
            json_escape(zh_path)
        ));
        for (position, (en, zh)) in en_sections.iter().zip(&zh_sections).enumerate() {
            let is_command = en.heading.trim_start_matches('#').trim().starts_with('`');
            if is_command {
                assert_eq!(
                    en.heading, zh.heading,
                    "{} 的第 {position} 段标题与 {} 不一致：命令标题必须逐字相同",
                    zh_path, en_path
                );
            }
            assert_eq!(
                en.paragraphs.len(),
                zh.paragraphs.len(),
                "{} 的 `{}` 段落数与 {} 不一致（英文 {}、中文 {}）——译文漏段或英文新增未译",
                zh_path,
                en.heading.trim_start_matches('#').trim(),
                en_path,
                en.paragraphs.len(),
                zh.paragraphs.len()
            );
            out.push_str(&format!(
                "        {{ \"en\": \"{}\", \"zh\": \"{}\", \"paragraphs\": {} }}{}\n",
                json_escape(en.heading),
                json_escape(zh.heading),
                en.paragraphs.len(),
                if position + 1 == en_sections.len() {
                    ""
                } else {
                    ","
                }
            ));
        }
        out.push_str(&format!(
            "      ]\n    }}{}\n",
            if index + 1 == pairs.len() { "" } else { "," }
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

/// 生成物与 `COMMANDS` 的一致性约束；生成器与用例共用。
pub fn check_consistency() {
    for command in declare::COMMANDS {
        // 写命令必须同时声明 `--yes`：确认门接受这个选项，且参考里要能看出它。
        if command.effect.is_write() {
            assert!(
                command.accepts("--yes"),
                "写命令 {} 没有声明 --yes；确认门无法成立",
                command.name
            );
        }
        for variant in command.variants {
            for option in variant.options {
                assert!(
                    OPTIONS.iter().any(|(known, _, _)| known == option),
                    "选项 {option} 缺少文档条目（命令 {}）",
                    command.name
                );
            }
        }
        for option in command.common_options {
            assert!(
                OPTIONS.iter().any(|(known, _, _)| known == option),
                "公共选项 {option} 缺少文档条目（命令 {}）",
                command.name
            );
        }
    }
}

const HEADER: &str = "Generated from `app/cli/declare.rs`, `app/report/exit.rs` and \
                      `diskedit_core::fs::catalog` by `docsgen`.\nRegenerate with \
                      `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`.\nCI compares the \
                      working tree with the generator output. Do not edit by hand.\n\n";

/// 选项的展示值与说明；顺序即文档顺序，生成前会按声明核对完整性。
const OPTIONS: &[(&str, &str, &str)] = &[
    (
        "--alias",
        "`NAME`",
        "name a target without WWN, EUI or repeatable serial (nbd, rbd, dm, md)",
    ),
    ("--align", "`A`", "placement alignment: mib|none"),
    (
        "--allow-move",
        "—",
        "relocate blocking partitions after printing the plan",
    ),
    (
        "--chunk",
        "`MiB`",
        "resume granularity in MiB, 1..=64; default 4 for new jobs",
    ),
    (
        "--discard-replay",
        "—",
        "confirm giving up a range `diskedit replay` could still replay",
    ),
    (
        "--dry-run",
        "—",
        "print the plan and stop before the first write; no --yes needed",
    ),
    ("--end", "`LBA`", "new end LBA for in-place resizing"),
    (
        "--fs-only",
        "—",
        "resize only the filesystem: grow it into its partition, or through a mountpoint",
    ),
    (
        "--grow-lv",
        "—",
        "also grow the LV: the one named by --lv, else the only LV",
    ),
    (
        "--grow-to-end",
        "—",
        "pin the end at the last usable LBA (fill semantics)",
    ),
    (
        "--json",
        "—",
        "print the structured per-partition detail instead of the text layout",
    ),
    (
        "--keep-partuuid",
        "—",
        "reuse the source PARTUUID instead of generating a new one",
    ),
    (
        "--label",
        "`S`",
        "filesystem label (omitted means an empty label, not a default)",
    ),
    ("--lv", "`NAME`", "the LV to grow (implies --grow-lv)"),
    (
        "--name",
        "`S`",
        "entry name for the partition this command creates",
    ),
    (
        "--no-fs",
        "—",
        "skip the filesystem step of the grown partition",
    ),
    (
        "--overwrite-data",
        "—",
        "accept overwriting bytes an existing partition holds",
    ),
    (
        "--part-number",
        "`N`",
        "1-based entry slot; it must be empty",
    ),
    (
        "--root",
        "`ROOT`",
        "scan a record container directory instead of listing a target's record",
    ),
    ("--random", "—", "ask the target to generate a new value"),
    (
        "--sector-size",
        "`N`",
        "logical sector size override for raw images (power of two, at most 65536)",
    ),
    (
        "--start",
        "`LBA|end|align`",
        "new start: an LBA, `end` for tail packing, or `align` for the next 1 MiB boundary",
    ),
    (
        "--table",
        "`gpt|mbr`",
        "partition table type: gpt (default) or mbr",
    ),
    (
        "--type-guid",
        "`GUID|0xNN`",
        "GPT type GUID text, or an MBR type byte 0xNN",
    ),
    ("--yes", "—", "acknowledge a write before it starts"),
];

fn reference() -> String {
    check_consistency();
    let mut out = String::from(HEADER);
    out.push_str(
        "# diskedit command reference\n\nWrite commands on a block device without a WWN, EUI or \
         repeatable serial\n(nbd, rbd, dm, md) need `--alias NAME`; images and devices with \
         an\nidentity of their own reject the flag.\n",
    );
    for (group, heading) in [
        (Group::Everyday, "## Commands"),
        (Group::LowLevel, "## Low-level"),
    ] {
        out.push_str(&format!("\n{heading}\n"));
        for command in declare::COMMANDS
            .iter()
            .filter(|command| command.group == group)
        {
            out.push_str(&format!(
                "\n### `{}`\n\n{}\n\n```text\n",
                command.name, command.summary
            ));
            for variant in command.variants {
                out.push_str(&format!("diskedit {} {}\n", command.name, variant.syntax));
            }
            out.push_str("```\n\n| property | value | options |\n|---|---|---|\n");
            let suffix = match command.variants[0].target {
                TargetSuffix::Any => "with or without a suffix",
                TargetSuffix::Whole => "no partition suffix",
                TargetSuffix::Partition => "`:N` or `:last` required",
            };
            let confirm = match command.effect {
                Effect::Write => "required (`--yes`)",
                Effect::ReadOnly => "none",
            };
            out.push_str(&format!(
                "| {suffix} | {confirm} | {} |\n",
                options_of(command)
            ));
        }
    }
    out.push_str("\n## options\n\n| option | value | description |\n|---|---|---|\n");
    for (option, value, description) in OPTIONS {
        out.push_str(&format!("| `{option}` | {value} | {description} |\n"));
    }
    out
}

fn options_of(command: &declare::CommandDef) -> String {
    let mut options: Vec<&str> = command.common_options.to_vec();
    for variant in command.variants {
        for option in variant.options {
            if !options.contains(option) {
                options.push(option);
            }
        }
    }
    options.sort_unstable();
    if options.is_empty() {
        return "—".to_owned();
    }
    options
        .iter()
        .map(|option| format!("`{option}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn capability_matrix() -> String {
    let mut out = String::from(HEADER);
    out.push_str(
        "# diskedit capability matrix\n\nOne row per content kind the tool identifies. A cell is \
         the support class\nreported by the filesystem catalogue; the steps each class runs are \
         declared\nthere as well.\n\n| content | grow | shrink | attributes | check | mkfs \
         |\n|---|---|---|---|---|---|\n",
    );
    for kind in ContentKind::ALL {
        let profile = catalog::profile(kind);
        let cells: Vec<&str> = [
            Verb::Grow,
            Verb::Shrink,
            Verb::Attrs,
            Verb::Check,
            Verb::Mkfs,
        ]
        .into_iter()
        .map(|verb| match profile.support(verb) {
            Support::Tools(_) => "supported",
            Support::NotApplicable => "not applicable",
            Support::Unsupported(_) => "unsupported",
        })
        .collect();
        out.push_str(&format!(
            "| `{}` | {} |\n",
            kind.as_str(),
            cells.join(" | ")
        ));
    }
    out.push_str("\n## Programs\n\nExternal programs the catalogue refers to, by identity:\n\n");
    let programs: Vec<String> = Program::ALL
        .iter()
        .map(|program| format!("`{program:?}`"))
        .collect();
    out.push_str(&programs.join(", "));
    let writable = crate::platform::WRITING_HOSTS.join(", ");
    out.push_str(&format!(
        "\n\n## Runtime gate\n\nWriting is supported on {writable} only. Elsewhere the dispatch \
         gate refuses\nany command whose effect is to write; read-only entries are admitted and run\nwhere the \
         host has a backend for them. Image files never need root for table writes;\nblock \
         devices do.\n\n| command | image file | block device | other platforms \
         |\n|---|---|---|---|\n"
    ));
    for command in declare::COMMANDS {
        let (image, block, other) = match command.effect {
            Effect::ReadOnly => ("yes", "yes", "yes"),
            Effect::Write => ("yes", "yes (root)", "refused (6)"),
        };
        out.push_str(&format!(
            "| `{}` | {image} | {block} | {other} |\n",
            command.name
        ));
    }
    out
}

fn exit_codes() -> String {
    let mut out = String::from(HEADER);
    out.push_str(
        "# diskedit exit status\n\nSeven values; the mapping lives in \
         `app/report/exit.rs`.\n\n| code | meaning |\n|---|---|\n",
    );
    for (code, meaning) in [
        (
            exit::SUCCESS,
            "success: nothing written and no record created, or the record was cleaned up and the \
             kernel view is in sync, or a full rollback completed",
        ),
        (
            exit::INTERNAL,
            "internal error or a broken invariant before any target byte was written",
        ),
        (exit::USAGE, "usage or parse error"),
        (
            exit::REFUSED,
            "request refused, record damaged, or evidence points at a foreign or damaged object",
        ),
        (
            exit::ENVIRONMENT,
            "I/O or environment failure after execution began, with no target bytes changed",
        ),
        (
            exit::PARTIAL,
            "partial completion, a record awaits recovery, or persistent state is reached while \
             the kernel view is stale",
        ),
        (exit::UNSUPPORTED, "capability unsupported"),
    ] {
        out.push_str(&format!("| {code} | {meaning} |\n"));
    }
    out
}
