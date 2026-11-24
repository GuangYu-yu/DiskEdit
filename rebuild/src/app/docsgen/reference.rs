//! 命令参考：从命令声明渲染用法、目标形态、确认要求与选项。

use super::{Lang, header};
use crate::app::cli::spec::{Group, SPECS, Spec, TargetMode};

/// 选项说明：名字、取值占位、英文与中文说明。
struct OptionDoc {
    name: &'static str,
    value: &'static str,
    en: &'static str,
    zh: &'static str,
}

const OPTIONS: &[OptionDoc] = &[
    OptionDoc {
        name: "--align",
        value: "A",
        en: "placement alignment: mib|none",
        zh: "放置对齐：mib|none",
    },
    OptionDoc {
        name: "--allow-move",
        value: "",
        en: "relocate blocking partitions after printing the plan",
        zh: "打印计划后搬移挡路的分区",
    },
    OptionDoc {
        name: "--chunk",
        value: "MiB",
        en: "resume granularity in MiB, 1..=64; default 4 for new jobs",
        zh: "恢复粒度，单位 MiB，1..=64；新任务默认 4",
    },
    OptionDoc {
        name: "--discard-replay",
        value: "",
        en: "confirm giving up a range `recover` could still replay",
        zh: "确认放弃 `recover` 仍可回放的区间",
    },
    OptionDoc {
        name: "--dry-run",
        value: "",
        en: "print the plan and stop before the first write; no --yes needed",
        zh: "打印计划并在第一次写入前停止；不需要 --yes",
    },
    OptionDoc {
        name: "--end",
        value: "LBA",
        en: "new end LBA for in-place resizing",
        zh: "就地扩容或收缩的新结束 LBA",
    },
    OptionDoc {
        name: "--grow",
        value: "",
        en: "append a relocation preview for partition :N",
        zh: "追加分区 :N 的搬移预览",
    },
    OptionDoc {
        name: "--grow-lv",
        value: "",
        en: "also grow the LV: the one named by --lv, else the only LV",
        zh: "同时扩容 LV：由 --lv 指定的一个，否则为唯一的 LV",
    },
    OptionDoc {
        name: "--grow-to-end",
        value: "",
        en: "pin the end at the last usable LBA (fill semantics)",
        zh: "把末端固定在最后一个可用 LBA（填充语义）",
    },
    OptionDoc {
        name: "--identity",
        value: "NAME",
        en: "name a target without WWN, EUI or repeatable serial (nbd, rbd, dm, md)",
        zh: "为不带 WWN、EUI 或可重复序列号的目标（nbd、rbd、dm、md）命名",
    },
    OptionDoc {
        name: "--keep-partuuid",
        value: "",
        en: "reuse the source PARTUUID instead of generating a new one",
        zh: "复用源的 PARTUUID，而不是生成新的",
    },
    OptionDoc {
        name: "--label",
        value: "S",
        en: "filesystem label (omitted means an empty label, not a default)",
        zh: "文件系统标签（省略即空标签，不是默认值）",
    },
    OptionDoc {
        name: "--lv",
        value: "NAME",
        en: "the LV to grow (implies --grow-lv)",
        zh: "要扩容的 LV（隐含 --grow-lv）",
    },
    OptionDoc {
        name: "--name",
        value: "S",
        en: "entry name for the partition this command creates",
        zh: "该命令创建的条目名",
    },
    OptionDoc {
        name: "--no-fs",
        value: "",
        en: "skip the filesystem step of the grown partition",
        zh: "跳过扩容分区的文件系统步骤",
    },
    OptionDoc {
        name: "--overwrite-data",
        value: "",
        en: "accept overwriting bytes an existing partition holds",
        zh: "接受覆盖既有分区所占据的字节",
    },
    OptionDoc {
        name: "--part-number",
        value: "N",
        en: "1-based entry slot; it must be empty",
        zh: "条目槽位，从 1 起；必须为空",
    },
    OptionDoc {
        name: "--random",
        value: "",
        en: "ask the target to generate a new value",
        zh: "要求目标生成新值",
    },
    OptionDoc {
        name: "--release",
        value: "",
        en: "drop the recovery files, keeping the writes they describe",
        zh: "丢弃恢复文件，保留它们所描述的写入",
    },
    OptionDoc {
        name: "--replay-table-only",
        value: "",
        en: "replay only the recorded table regions of a completed move",
        zh: "只回放一次数据已搬移完成的移动操作所记录的表区域",
    },
    OptionDoc {
        name: "--sector-size",
        value: "N",
        en: "logical sector size override for raw images (power of two, at most 65536)",
        zh: "raw 镜像的逻辑扇区大小覆盖值（2 的幂，最大 65536 字节）",
    },
    OptionDoc {
        name: "--start",
        value: "LBA|end|align",
        en: "new start: an LBA, `end` for tail packing, or `align` for the next 1 MiB boundary",
        zh: "新起点：一个 LBA，`end` 表示尾部贴合，或 `align` 表示下一个 1 MiB 边界",
    },
    OptionDoc {
        name: "--table",
        value: "gpt|mbr",
        en: "partition table type: gpt (default) or mbr",
        zh: "分区表类型：gpt（默认）或 mbr",
    },
    OptionDoc {
        name: "--type-guid",
        value: "GUID|0xNN",
        en: "GPT type GUID text, or an MBR type byte 0xNN",
        zh: "GPT 类型 GUID 文本，或 MBR 类型字节 0xNN",
    },
    OptionDoc {
        name: "--yes",
        value: "",
        en: "acknowledge a write before it starts",
        zh: "写入开始前予以确认",
    },
];

/// 命令概要的中文译本；英文概要直接取 `Spec::summary`。
const ZH_SUMMARIES: &[(&str, &str)] = &[
    ("info", "显示目标与分区概况"),
    ("inspect", "逐分区显示起止与内容"),
    ("records list", "列出记录目录条目"),
    ("records verify", "校验一条记录"),
    ("records scan", "扫描记录目录"),
    ("resize", "改变分区大小"),
    ("move", "平移一个分区"),
    ("copy", "复制分区到新位置并新增分区"),
    ("delete", "删除一个分区"),
    ("set", "修改分区或文件系统属性"),
    ("check", "检查一个分区的文件系统"),
    ("mkfs", "在分区上建卷"),
    ("resizefs", "调整分区内文件系统大小"),
    ("recover", "回滚、重放或放弃记录"),
    ("new", "新建分区表"),
    ("add", "新增一个分区"),
];

fn summary_of(spec: &Spec, lang: Lang) -> &'static str {
    match lang {
        Lang::En => spec.summary,
        Lang::Zh => ZH_SUMMARIES
            .iter()
            .find(|(name, _)| *name == spec.name)
            .map(|(_, text)| *text)
            .expect("每条命令声明都必须有中文概要"),
    }
}

/// 渲染一份语言对应的命令参考。
pub fn render(lang: Lang) -> String {
    let mut out = String::new();
    out.push_str(&header(lang));
    out.push_str(match lang {
        Lang::En => {
            "# diskedit command reference\n\n\
                     Write commands on a block device without a WWN, EUI or repeatable serial\n\
                     (nbd, rbd, dm, md) need `--identity NAME`; images and devices with an\n\
                     identity of their own reject the flag.\n\n"
        }
        Lang::Zh => {
            "# diskedit 命令参考\n\n\
                     对不带 WWN、EUI 或可重复序列号的块设备（nbd、rbd、dm、md），写命令必须给出\n\
                     `--identity NAME`；镜像与自带身份的设备拒绝该旗标。\n\n"
        }
    });

    for (group, title) in group_titles(lang) {
        out.push_str(&format!("## {title}\n\n"));
        for spec in SPECS.iter().filter(|spec| spec.group == group) {
            render_command(&mut out, spec, lang);
        }
    }

    out.push_str(&format!("## {}\n\n", table_heading(lang, Heading::Options)));
    out.push_str(&format!(
        "| {} | {} | {} |\n|---|---|---|\n",
        table_heading(lang, Heading::Option),
        table_heading(lang, Heading::Value),
        table_heading(lang, Heading::Description)
    ));
    for option in OPTIONS {
        let value = if option.value.is_empty() {
            "—".to_owned()
        } else {
            format!("`{}`", option.value)
        };
        let text = match lang {
            Lang::En => option.en,
            Lang::Zh => option.zh,
        };
        out.push_str(&format!("| `{}` | {value} | {text} |\n", option.name));
    }
    out.push('\n');
    out
}

fn render_command(out: &mut String, spec: &Spec, lang: Lang) {
    let summary = summary_of(spec, lang);
    out.push_str(&format!("### `{}`\n\n{summary}\n\n", spec.name));
    out.push_str("```text\n");
    for usage in spec.usage {
        out.push_str(usage);
        out.push('\n');
    }
    out.push_str("```\n\n");
    out.push_str(&format!(
        "| {} | {} | {} |\n|---|---|---|\n",
        table_heading(lang, Heading::Property),
        table_heading(lang, Heading::Value),
        table_heading(lang, Heading::Options)
    ));
    out.push_str(&format!(
        "| {} | {} | {} |\n\n",
        target_label(spec.target, lang),
        confirmation_label(spec.confirmation, lang),
        option_list(spec)
    ));
}

fn group_titles(lang: Lang) -> [(Group, &'static str); 2] {
    match lang {
        Lang::En => [
            (Group::Everyday, "Commands"),
            (Group::LowLevel, "Low-level"),
        ],
        Lang::Zh => [(Group::Everyday, "命令"), (Group::LowLevel, "底层命令")],
    }
}

/// 表头种类。
#[derive(Clone, Copy)]
enum Heading {
    Options,
    Option,
    Value,
    Description,
    Property,
}

fn table_heading(lang: Lang, key: Heading) -> &'static str {
    match (lang, key) {
        (Lang::En, Heading::Options) => "options",
        (Lang::En, Heading::Option) => "option",
        (Lang::En, Heading::Value) => "value",
        (Lang::En, Heading::Description) => "description",
        (Lang::En, Heading::Property) => "property",
        (Lang::Zh, Heading::Options) => "选项",
        (Lang::Zh, Heading::Option) => "选项",
        (Lang::Zh, Heading::Value) => "取值",
        (Lang::Zh, Heading::Description) => "说明",
        (Lang::Zh, Heading::Property) => "属性",
    }
}

fn target_label(target: TargetMode, lang: Lang) -> &'static str {
    match (target, lang) {
        (TargetMode::Whole, Lang::En) => "no partition suffix",
        (TargetMode::Whole, Lang::Zh) => "不带分区后缀",
        (TargetMode::Partition, Lang::En) => "`:N` or `:last` required",
        (TargetMode::Partition, Lang::Zh) => "必须带 `:N` 或 `:last`",
        (TargetMode::Any, Lang::En) => "with or without a suffix",
        (TargetMode::Any, Lang::Zh) => "带或不带后缀",
        (TargetMode::None, Lang::En) => "no target",
        (TargetMode::None, Lang::Zh) => "无目标",
    }
}

fn confirmation_label(confirmation: bool, lang: Lang) -> &'static str {
    match (confirmation, lang) {
        (true, Lang::En) => "required (`--yes`)",
        (true, Lang::Zh) => "必须（`--yes`）",
        (false, Lang::En) => "none",
        (false, Lang::Zh) => "无",
    }
}

fn option_list(spec: &Spec) -> String {
    if spec.options.is_empty() {
        return "—".to_owned();
    }
    spec.options
        .iter()
        .map(|option| format!("`{}`", option.name))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_option_has_a_description() {
        for spec in SPECS {
            for option in spec.options {
                assert!(
                    OPTIONS.iter().any(|doc| doc.name == option.name),
                    "{} 的选项 {} 缺少说明",
                    spec.name,
                    option.name
                );
            }
        }
    }

    #[test]
    fn every_documented_option_is_declared_somewhere() {
        for doc in OPTIONS {
            assert!(
                SPECS
                    .iter()
                    .any(|spec| spec.options.iter().any(|option| option.name == doc.name)),
                "{} 有说明但没有命令声明它",
                doc.name
            );
        }
    }

    #[test]
    fn every_command_has_a_chinese_summary() {
        for spec in SPECS {
            assert!(
                ZH_SUMMARIES.iter().any(|(name, _)| *name == spec.name),
                "{} 缺少中文概要",
                spec.name
            );
        }
    }

    #[test]
    fn reference_lists_every_entry_in_both_languages() {
        for lang in Lang::ALL {
            let text = render(lang);
            for spec in SPECS {
                assert!(
                    text.contains(&format!("### `{}`", spec.name)),
                    "{} 缺少 {}",
                    lang.dir(),
                    spec.name
                );
            }
        }
    }
}
