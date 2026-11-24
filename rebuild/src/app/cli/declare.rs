//! 命令面声明：16 个入口、各自的调用形态与选项白名单。
//!
//! 帮助、用法校验与导出到 `target/command-surface.json` 的命令面都读这同一张表；
//! 声明与实现不一致时，门禁的对照会直接暴露。声明只写数据，不含任何目标事实。

/// 命令在顶层帮助里的分组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Everyday,
    LowLevel,
}

/// 确认门：写命令在解析期必须拿到 `--yes`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// 解析期必须有 `--yes`。
    Required,
    /// 不接受 `--yes`。
    None,
}

/// 一个形态对目标后缀的要求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetSuffix {
    /// 带不带 `:N` 都可以。
    Any,
    /// 必须不带 `:N`。
    Whole,
    /// 必须带 `:N`（或 `:last`）。
    Partition,
}

/// 形态判定项：某个旗标出现，或某个位置参数等于字面量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selector {
    Flag(&'static str),
    Positional { index: usize, literal: &'static str },
}

impl Selector {
    /// 导出到命令面 JSON 的稳定文本。
    pub fn describe(self) -> String {
        match self {
            Self::Flag(flag) => format!("flag:{flag}"),
            Self::Positional { index, literal } => format!("pos:{index}={literal}"),
        }
    }
}

/// 一个命令的一种调用形态。
#[derive(Clone, Copy, Debug)]
pub struct Variant {
    /// `diskedit {name} ` 之后的部分。
    pub syntax: &'static str,
    /// 形态自己的选项；命令级公共选项另见 [`CommandDef::common_options`]。
    pub options: &'static [&'static str],
    /// 全部成立才命中；空表示"没有判别项"。
    pub selects: &'static [Selector],
    pub target: TargetSuffix,
    /// 无判别项命中时采用这一形态；每个命令至多一个。
    pub default: bool,
}

const fn variant(
    syntax: &'static str,
    options: &'static [&'static str],
    selects: &'static [Selector],
    target: TargetSuffix,
    default: bool,
) -> Variant {
    Variant {
        syntax,
        options,
        selects,
        target,
        default,
    }
}

/// 一条命令的全部声明。
#[derive(Clone, Copy, Debug)]
pub struct CommandDef {
    pub name: &'static str,
    pub group: Group,
    pub summary: &'static str,
    pub variants: &'static [Variant],
    pub common_options: &'static [&'static str],
    pub confirmation: Confirmation,
}

impl CommandDef {
    /// 该命令是否声明了这个选项（任一形态或公共选项）。
    pub fn accepts(&self, flag: &str) -> bool {
        self.common_options.contains(&flag)
            || self
                .variants
                .iter()
                .any(|variant| variant.options.contains(&flag))
    }

    /// 某个形态的白名单：形态自己的选项加命令级公共选项。
    pub fn accepts_in(&self, variant: &Variant, flag: &str) -> bool {
        self.common_options.contains(&flag) || variant.options.contains(&flag)
    }
}

/// 全部命令声明，顺序即帮助与命令面的顺序。
pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "info",
        group: Group::Everyday,
        summary: "show partition table and per-partition filesystem identification",
        variants: &[
            variant(
                "<TARGET> [--sector-size N]",
                &[],
                &[],
                TargetSuffix::Whole,
                true,
            ),
            variant(
                "<TARGET>:N --grow [--sector-size N]",
                &["--grow"],
                &[Selector::Flag("--grow")],
                TargetSuffix::Partition,
                false,
            ),
        ],
        common_options: &["--sector-size"],
        confirmation: Confirmation::None,
    },
    CommandDef {
        name: "inspect",
        group: Group::Everyday,
        summary: "show per-partition extents and content",
        variants: &[variant(
            "<TARGET|PATH>",
            &[],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &["--identity"],
        confirmation: Confirmation::None,
    },
    CommandDef {
        name: "records list",
        group: Group::Everyday,
        summary: "list recovery record directory entries",
        variants: &[variant("[TARGET]", &[], &[], TargetSuffix::Whole, true)],
        common_options: &["--identity"],
        confirmation: Confirmation::None,
    },
    CommandDef {
        name: "records verify",
        group: Group::Everyday,
        summary: "verify one recovery record",
        variants: &[variant(
            "<TARGET> <ID>",
            &[],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &["--identity"],
        confirmation: Confirmation::None,
    },
    CommandDef {
        name: "records scan",
        group: Group::Everyday,
        summary: "scan a recovery record container",
        variants: &[variant("<ROOT>", &[], &[], TargetSuffix::Any, true)],
        common_options: &[],
        confirmation: Confirmation::None,
    },
    CommandDef {
        name: "resize",
        group: Group::Everyday,
        summary: "resize a partition and its filesystem",
        variants: &[
            variant(
                "<TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes",
                &["--allow-move", "--chunk", "--dry-run", "--grow-lv", "--lv"],
                &[],
                TargetSuffix::Any,
                true,
            ),
            variant(
                "<TARGET>:N --end LBA [--align A] [--no-fs] --yes",
                &["--dry-run", "--end", "--align"],
                &[Selector::Flag("--end")],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N --grow-to-end [--align A] [--no-fs] --yes",
                &["--dry-run", "--grow-to-end", "--align"],
                &[Selector::Flag("--grow-to-end")],
                TargetSuffix::Partition,
                false,
            ),
        ],
        common_options: &["--no-fs", "--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "move",
        group: Group::Everyday,
        summary: "relocate a partition",
        variants: &[variant(
            "<TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]",
            &["--align", "--chunk", "--start"],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "copy",
        group: Group::Everyday,
        summary: "copy a partition to a new location and add its entry",
        variants: &[variant(
            "<TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]",
            &["--align", "--chunk", "--keep-partuuid", "--name", "--start"],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "delete",
        group: Group::Everyday,
        summary: "delete a partition entry",
        variants: &[variant(
            "<TARGET>:N --yes",
            &[],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "set",
        group: Group::Everyday,
        summary: "set a partition or filesystem attribute",
        variants: &[
            variant(
                "<TARGET>:N name S --yes",
                &[],
                &[Selector::Positional {
                    index: 1,
                    literal: "name",
                }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N label S --yes",
                &[],
                &[Selector::Positional {
                    index: 1,
                    literal: "label",
                }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N uuid [U] --yes [--random]",
                &[],
                &[Selector::Positional {
                    index: 1,
                    literal: "uuid",
                }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N flag F on|off --yes",
                &[],
                &[Selector::Positional {
                    index: 1,
                    literal: "flag",
                }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N type GUID|0xXX --yes",
                &[],
                &[Selector::Positional {
                    index: 1,
                    literal: "type",
                }],
                TargetSuffix::Partition,
                false,
            ),
        ],
        common_options: &["--identity", "--random", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "check",
        group: Group::Everyday,
        summary: "check the filesystem in a partition",
        variants: &[variant(
            "<TARGET>:N",
            &[],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &["--sector-size"],
        confirmation: Confirmation::None,
    },
    CommandDef {
        name: "mkfs",
        group: Group::Everyday,
        summary: "create a filesystem in a partition",
        variants: &[variant(
            "<TARGET>:N <FS> --yes [--label S]",
            &["--label"],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "resizefs",
        group: Group::Everyday,
        summary: "resize the filesystem inside a partition",
        variants: &[
            variant(
                "<TARGET>:N [BYTES] --yes",
                &[],
                &[],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<MOUNTPOINT> [BYTES] --yes",
                &[],
                &[],
                TargetSuffix::Whole,
                false,
            ),
        ],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "recover",
        group: Group::Everyday,
        summary: "roll back, replay or release a recovery record",
        variants: &[
            variant("<TARGET> --yes", &[], &[], TargetSuffix::Whole, true),
            variant(
                "<TARGET> --replay-table-only --yes",
                &["--replay-table-only"],
                &[Selector::Flag("--replay-table-only")],
                TargetSuffix::Whole,
                false,
            ),
            variant(
                "<TARGET> --release [--discard-replay] --yes",
                &["--release", "--discard-replay"],
                &[Selector::Flag("--release")],
                TargetSuffix::Whole,
                false,
            ),
        ],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "new",
        group: Group::LowLevel,
        summary: "create a fresh partition table",
        variants: &[variant(
            "<TARGET> [--table gpt|mbr] --yes [--overwrite-data]",
            &["--table", "--overwrite-data"],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
    CommandDef {
        name: "add",
        group: Group::LowLevel,
        summary: "add a partition",
        variants: &[variant(
            "<TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes",
            &[
                "--align",
                "--name",
                "--part-number",
                "--start",
                "--type-guid",
            ],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &["--identity", "--sector-size", "--yes"],
        confirmation: Confirmation::Required,
    },
];

/// 按名字查声明；`records` 的子命令用空格拼接。
pub fn find(name: &str) -> Option<&'static CommandDef> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// 全部入口名，按声明顺序。
pub fn declared_entries() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|command| command.name)
}

/// 命令面 JSON：供门禁与旧版逐项对照。
///
/// 结构固定为 `{"commands":[{"name","confirmation","common_options","variants":[...]}]}`；
/// 每个形态带 `syntax`、`options`、`selectors`、`target`、`default` 与
/// `syntax_only_options`。
pub fn surface_json() -> String {
    let commands: Vec<serde_json::Value> = COMMANDS
        .iter()
        .map(|command| {
            let variants: Vec<serde_json::Value> = command
                .variants
                .iter()
                .map(|variant| {
                    let mut options: Vec<String> = variant
                        .options
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect();
                    options.sort();

                    let mut selectors: Vec<String> = variant
                        .selects
                        .iter()
                        .map(|selector| selector.describe())
                        .collect();
                    selectors.sort();

                    let mut mentioned: Vec<String> = options_in_syntax(variant.syntax);
                    mentioned.retain(|option| !variant.options.contains(&option.as_str()));
                    mentioned.sort();
                    mentioned.dedup();

                    serde_json::json!({
                        "syntax": variant.syntax,
                        "options": options,
                        "selectors": selectors,
                        "target": target_name(variant.target),
                        "default": variant.default,
                        "syntax_only_options": mentioned,
                    })
                })
                .collect();

            let mut common: Vec<&str> = command.common_options.to_vec();
            common.sort();

            serde_json::json!({
                "name": command.name,
                "confirmation": confirmation_name(command.confirmation),
                "common_options": common,
                "variants": variants,
            })
        })
        .collect();

    let document = serde_json::json!({ "commands": commands });
    serde_json::to_string_pretty(&document).expect("命令面只含静态数据，序列化不会失败")
}

fn target_name(target: TargetSuffix) -> &'static str {
    match target {
        TargetSuffix::Any => "Any",
        TargetSuffix::Whole => "Whole",
        TargetSuffix::Partition => "Partition",
    }
}

fn confirmation_name(confirmation: Confirmation) -> &'static str {
    match confirmation {
        Confirmation::Required => "Required",
        Confirmation::None => "None",
    }
}

/// 语法串里出现的选项，去掉 `--yes`。
fn options_in_syntax(syntax: &str) -> Vec<String> {
    let bytes = syntax.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == b'-' && bytes[index + 1] == b'-' {
            let start = index;
            let mut end = index + 2;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-') {
                end += 1;
            }
            let token = &syntax[start..end];
            if token.len() > 2 && token != "--yes" {
                found.push(token.to_owned());
            }
            index = end;
        } else {
            index += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_declares_a_default_variant_at_most_once() {
        for command in COMMANDS {
            let defaults = command
                .variants
                .iter()
                .filter(|variant| variant.default)
                .count();
            assert!(defaults <= 1, "{} 声明了多个默认形态", command.name);
        }
    }

    #[test]
    fn option_whitelists_are_unique_within_one_list() {
        for command in COMMANDS {
            let lists = std::iter::once(command.common_options)
                .chain(command.variants.iter().map(|variant| variant.options));
            for list in lists {
                let mut seen = Vec::new();
                for option in list {
                    assert!(
                        !seen.contains(option),
                        "{} 在同一白名单里重复声明选项 {option}",
                        command.name
                    );
                    seen.push(*option);
                }
            }
        }
    }

    #[test]
    fn syntax_scan_ignores_yes_and_keeps_other_flags() {
        assert_eq!(
            options_in_syntax("<TARGET>:N --grow [--sector-size N] --yes"),
            ["--grow", "--sector-size"]
        );
        assert_eq!(options_in_syntax("<TARGET> --yes"), Vec::<String>::new());
    }
}
