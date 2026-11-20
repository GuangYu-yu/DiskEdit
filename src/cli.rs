//! 命令行 schema 与解析：每条命令一份 [`CommandDef`]，解析产物是类型化 [`Request`]。
//!
//! 参数的位置由 `positionals` 里的槽位名声明，命令层按槽位名取值。
//! 每个槽位与每个旗标各自声明一份 [`ValueDomain`]：解析器只按它校验文本，
//! 命令层拿到的是已经解析好的类型化取值。

use crate::dev::PartSelector;
use crate::domain::{Align, SectorSize};
use crate::outcome::{EXIT_OK, EXIT_USAGE};
use crate::support::{bail_fail, exit_with, parse_guid, RunOutcome};

/// 用法错误打到 stderr 退 2；主动请求帮助打到 stdout 退 0
pub(crate) fn usage() -> ! {
    eprintln!("{}", crate::docs::usage_text());
    exit_with(EXIT_USAGE)
}

/// 主动请求帮助（`help` 无主题、`--help`、`-h`）的出口：请求有效，退 0
pub(crate) fn usage_help() -> ! {
    println!("{}", crate::docs::usage_text());
    exit_with(EXIT_OK)
}

/// 单命令详助；未知主题 → 顶层 usage
pub(crate) fn help_cmd(name: &str) -> ! {
    match crate::docs::help_text(name) {
        Some(text) => println!("{text}"),
        None => usage(),
    }
    exit_with(EXIT_OK)
}

pub(crate) fn version_help() -> ! {
    println!("diskedit {}", env!("CARGO_PKG_VERSION"));
    exit_with(EXIT_OK)
}

/// 一个值的语法。解析器只按声明在这里的值域校验；`Bool` 表示旗标不带值。
/// 占位符是帮助、错误文案与选项总表对账共用的那一个字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueDomain {
    Bool,
    /// 任意文本；携带帮助里的占位符
    Text(&'static str),
    /// 目标路径 + 可选 `:N`
    Target,
    /// 记录编号：32 位小写十六进制
    RecordId,
    /// `LBA`、`end` 或 `align`
    StartOffset,
    /// 十进制 LBA
    Lba,
    Align,
    SectorSize,
    /// 1..=64 MiB（搬移作业的分块）
    Chunk,
    /// `gpt` 或 `mbr`
    TableKind,
    /// 分区类型：GPT 用 GUID 文本（输入接受大写），MBR 用 `0xNN`
    TypeGuid,
    /// 分区号（u32）
    PartNumber,
    /// resize 的 SIZE：绝对值 / 增量 / 百分比 / `grow`
    ResizeSize,
    /// 绝对字节数（`--size` 与 resizefs 的 BYTES）
    AbsoluteSize,
}

impl ValueDomain {
    pub(crate) fn takes_value(self) -> bool {
        self != Self::Bool
    }

    pub(crate) fn placeholder(self) -> &'static str {
        match self {
            Self::Bool => "—",
            Self::Text(placeholder) => placeholder,
            Self::Target => "TARGET",
            Self::RecordId => "ID",
            Self::StartOffset => "LBA|end|align",
            Self::Lba => "LBA",
            Self::Align => "A",
            Self::SectorSize => "N",
            Self::Chunk => "MiB",
            Self::TableKind => "gpt|mbr",
            Self::TypeGuid => "GUID|0xNN",
            Self::PartNumber => "N",
            Self::ResizeSize | Self::AbsoluteSize => "SIZE",
        }
    }

    /// 值域的唯一实现。错误串是括号里的说明文字，由调用方补上旗标/槽位名。
    fn parse(self, text: &str) -> Result<Parsed, String> {
        match self {
            Self::Bool => Err("it takes no value".to_string()),
            Self::Text(_) => Ok(Parsed::Text(text.to_string())),
            Self::Target => crate::dev::parse_target(text)
                .map(|(target, part)| Parsed::Target { target, part })
                .map_err(|e| e.to_string()),
            Self::RecordId => {
                if text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                    Ok(Parsed::Text(text.to_string()))
                } else {
                    Err("a 32-digit lowercase hex record id, e.g. 0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string())
                }
            }
            Self::StartOffset => Ok(Parsed::Start(if text.eq_ignore_ascii_case("end") {
                StartSelector::PinnedToEnd
            } else if text.eq_ignore_ascii_case("align") {
                StartSelector::Aligned
            } else {
                StartSelector::Lba(text.parse().map_err(|_| "an LBA, `end` or `align`, e.g. 2048".to_string())?)
            })),
            Self::Lba => Ok(Parsed::Lba(
                text.parse().map_err(|_| "an LBA, e.g. 2048".to_string())?,
            )),
            Self::Align => Align::parse(text).map(Parsed::AlignValue).map_err(|e| e.to_string()),
            Self::SectorSize => {
                let bytes: u64 = text.parse().map_err(|_| "bytes, e.g. 4096".to_string())?;
                SectorSize::try_new(bytes).map(Parsed::SectorSizeValue).map_err(|e| e.to_string())
            }
            Self::Chunk => {
                let mib: u64 = text.parse().map_err(|_| "MiB, e.g. 4".to_string())?;
                if !(1..=64).contains(&mib) {
                    return Err("must be in 1..=64 MiB, e.g. 4".to_string());
                }
                Ok(Parsed::Mebibytes(mib))
            }
            Self::TableKind => crate::table::TableKind::parse(text)
                .map(Parsed::TableKind)
                .ok_or_else(|| "gpt|mbr".to_string()),
            Self::TypeGuid => parse_type_guid(text).map(Parsed::Type),
            Self::PartNumber => text
                .parse()
                .map(Parsed::PartNumber)
                .map_err(|_| "a partition number, e.g. 3".to_string()),
            Self::ResizeSize => parse_resize_size(text),
            Self::AbsoluteSize => match parse_size_delta(text) {
                Some(SizeArg::Bytes { kind: 0, bytes }) => Ok(Parsed::Absolute(bytes)),
                Some(_) => Err("absolute size only: a new partition has no previous size to be relative to; use 32M, not +32M/-32M/32%".to_string()),
                None => Err("use 10G | 500M | 1.5G | plain bytes, e.g. 33554432".to_string()),
            },
        }
    }
}

/// 值域解析后的类型化取值。多数 builder 只从这些变体取字段、不看原始文本；
/// `resizefs` 的离线形式例外，会把 BYTES 原文一并交给命令层（先按挂载点/离线分流再解析）。
#[derive(Debug)]
enum Parsed {
    Text(String),
    Target { target: String, part: Option<PartSelector> },
    Start(StartSelector),
    Lba(u64),
    AlignValue(Align),
    SectorSizeValue(SectorSize),
    Mebibytes(u64),
    TableKind(crate::table::TableKind),
    Type(TypeSelector),
    PartNumber(u32),
    ResizeSize(SizeSpec),
    Absolute(u64),
}

/// 旗标的落点：请求里的哪个字段被这个旗标设置。解析器按它写入 [`Options`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Yes,
    NoFs,
    AllowMove,
    GrowLv,
    Random,
    KeepPartuuid,
    OverwriteData,
    GrowToEnd,
    Grow,
    DryRun,
    DiscardReplay,
    ReplayTableOnly,
    Release,
    AlignValue,
    ChunkSize,
    SectorSize,
    Start,
    End,
    Table,
    Name,
    Lv,
    TypeGuid,
    Label,
    PartNumber,
}

/// 一个命令行旗标：名字、值域、帮助说明与落点。值域为 [`ValueDomain::Bool`] 即不带值。
///
/// `desc` 是这条旗标的说明来源；命令特有的补充写在 [`OptionDef::desc`]，两者不重复。
pub(crate) struct FlagSpec {
    pub(crate) name: &'static str,
    pub(crate) domain: ValueDomain,
    pub(crate) desc: &'static str,
    effect: Effect,
    /// 必须同时出现的其它旗标。解析期按用法错误拒绝缺项，命令层不再重判
    requires: &'static [&'static str],
}

const fn flag(
    name: &'static str,
    domain: ValueDomain,
    desc: &'static str,
    effect: Effect,
) -> FlagSpec {
    FlagSpec { name, domain, desc, effect, requires: &[] }
}

/// 旗标注册表：解析器判重、缺值文案、命令白名单与帮助/参考文档都读这一份。
pub(crate) const FLAGS: &[FlagSpec] = &[
    flag(
        "--align",
        ValueDomain::Align,
        "placement alignment: mib|none. add's automatic placement defaults to 1 MiB; an explicit --start (add/move/copy) and the end computed by resize align only when --align is given; move's `--start align` is always 1 MiB",
        Effect::AlignValue,
    ),
    flag(
        "--allow-move",
        ValueDomain::Bool,
        "permit relocating the partitions in the way: the plan is printed, then the write proceeds",
        Effect::AllowMove,
    ),
    flag(
        "--chunk",
        ValueDomain::Chunk,
        "recovery granularity in MiB, 1..=64; a resumed job keeps the record's value, a new job uses 4",
        Effect::ChunkSize,
    ),
    FlagSpec {
        name: "--discard-replay",
        domain: ValueDomain::Bool,
        desc: "confirm giving up the ranges `recover` could still replay",
        effect: Effect::DiscardReplay,
        requires: &["--release"],
    },
    flag(
        "--dry-run",
        ValueDomain::Bool,
        "print the plan and stop before the first write; --yes is not required",
        Effect::DryRun,
    ),
    flag(
        "--end",
        ValueDomain::Lba,
        "new end LBA for `resize` (grow or shrink in place)",
        Effect::End,
    ),
    flag(
        "--grow-lv",
        ValueDomain::Bool,
        "also grow the LV: the one named by --lv, else the only LV",
        Effect::GrowLv,
    ),
    flag(
        "--grow-to-end",
        ValueDomain::Bool,
        "pin the end at the last usable LBA (fill semantics)",
        Effect::GrowToEnd,
    ),
    flag(
        "--grow",
        ValueDomain::Bool,
        "append the relocation preview for partition :N",
        Effect::Grow,
    ),
    flag(
        "--keep-partuuid",
        ValueDomain::Bool,
        "reuse the source PARTUUID instead of generating a new one",
        Effect::KeepPartuuid,
    ),
    flag(
        "--label",
        ValueDomain::Text("S"),
        "filesystem label (omitted = an empty label, not a default)",
        Effect::Label,
    ),
    flag(
        "--lv",
        ValueDomain::Text("NAME"),
        "which LV to grow (implies --grow-lv)",
        Effect::Lv,
    ),
    flag(
        "--name",
        ValueDomain::Text("S"),
        "name for the entry this command creates",
        Effect::Name,
    ),
    flag(
        "--no-fs",
        ValueDomain::Bool,
        "skip the filesystem step of the grown partition",
        Effect::NoFs,
    ),
    flag(
        "--overwrite-data",
        ValueDomain::Bool,
        "accept overwriting bytes an existing partition holds",
        Effect::OverwriteData,
    ),
    flag(
        "--part-number",
        ValueDomain::PartNumber,
        "1-based entry slot; it must be empty",
        Effect::PartNumber,
    ),
    flag(
        "--random",
        ValueDomain::Bool,
        "ask the target to generate a new value",
        Effect::Random,
    ),
    flag(
        "--release",
        ValueDomain::Bool,
        "drop the recovery files, keeping the writes they describe",
        Effect::Release,
    ),
    flag(
        "--replay-table-only",
        ValueDomain::Bool,
        "allow `recover` to replay only the recorded table regions of a move whose data has already moved; the data those ranges name is not restored",
        Effect::ReplayTableOnly,
    ),
    flag(
        "--sector-size",
        ValueDomain::SectorSize,
        "logical sector size override for raw images (power of two, max 65536 bytes — an implementation limit, not a format requirement)",
        Effect::SectorSize,
    ),
    flag(
        "--start",
        ValueDomain::StartOffset,
        "new start: an LBA, `end` for pack-to-end, or `align` for the next 1 MiB boundary (move only)",
        Effect::Start,
    ),
    flag(
        "--table",
        ValueDomain::TableKind,
        "table kind: gpt (default) or mbr",
        Effect::Table,
    ),
    flag(
        "--type-guid",
        ValueDomain::TypeGuid,
        "GPT: type GUID text, e.g. 0fc63daf-8483-4772-8e79-3d69d8477de4; MBR: type byte 0xNN",
        Effect::TypeGuid,
    ),
    flag(
        "--yes",
        ValueDomain::Bool,
        "acknowledge a write before it starts",
        Effect::Yes,
    ),
];

pub(crate) fn flag_spec(name: &str) -> Option<&'static FlagSpec> {
    FLAGS.iter().find(|f| f.name == name)
}

/// `--start` 的三种给法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartSelector {
    Lba(u64),
    /// `--start end`：贴着可用区尾部打包
    PinnedToEnd,
    /// `--start align`：从该分区现有起点上取整到下一个 1 MiB 边界（仅 move）
    Aligned,
}

/// `--type-guid` 的两种形态：GPT 的类型 GUID，MBR 的类型字节。
/// 表型在命令层才判，解析期只区分写法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeSelector {
    Guid([u8; 16]),
    Mbr(u8),
}

/// `resize` 的 SIZE。百分比与增量都保留原样，锚定值由锁下的当前尺寸给出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeSpec {
    Grow,
    Absolute(u64),
    Delta { grow: bool, bytes: u64 },
    Percent { grow: bool, num: u64, denom: u64 },
}

impl SizeSpec {
    /// 错误文案里的原样回显
    pub(crate) fn text(self) -> String {
        match self {
            Self::Grow => "grow".to_string(),
            Self::Absolute(b) => b.to_string(),
            Self::Delta { grow: true, bytes } => format!("+{bytes}"),
            Self::Delta { grow: false, bytes } => format!("-{bytes}"),
            Self::Percent { grow, num, denom } => {
                let sign = if grow { "+" } else { "-" };
                format!("{sign}{}%", decimal(num, denom))
            }
        }
    }
}

/// 分子/分母的十进制写法，供百分比回显；不参与任何计算
fn decimal(num: u64, denom: u64) -> String {
    let int = num / denom;
    let rem = num % denom;
    if rem == 0 {
        return int.to_string();
    }
    let mut frac = String::new();
    let mut r = rem;
    for _ in 0..18 {
        r *= 10;
        frac.push(char::from(b'0' + (r / denom) as u8));
        r %= denom;
        if r == 0 {
            break;
        }
    }
    format!("{int}.{frac}")
}

/// 位置参数的语义槽位。命令层按槽位名取值，不按下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Slot {
    Target,
    /// `records list` 的可选目标：省略时列主机仓库里的全部记录
    OptionalTarget,
    /// `records verify` 的记录编号：32 位小写十六进制
    RecordId,
    /// `records scan` 的容器根目录：按目录读，不拆分区后缀
    RootDir,
    /// `resize` 的 SIZE / `resizefs` 的 BYTES：`-` 开头的相对尺寸只在这里放行
    Size,
    Property,
    Value,
    State,
    FsType,
}

/// 一个位置参数：槽位、帮助与错误消息里的名字、值域，以及在形态内是否可省。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PositionalDef {
    pub(crate) slot: Slot,
    pub(crate) name: &'static str,
    pub(crate) domain: ValueDomain,
    /// 可省只允许出现在尾部：缺省由 `build` 按 `None` 收口
    pub(crate) optional: bool,
}

const fn positional(slot: Slot, name: &'static str, domain: ValueDomain, optional: bool) -> PositionalDef {
    PositionalDef { slot, name, domain, optional }
}

const TARGET_ARG: PositionalDef =
    positional(Slot::Target, "TARGET", ValueDomain::Target, false);
const OPTIONAL_TARGET_ARG: PositionalDef =
    positional(Slot::OptionalTarget, "TARGET", ValueDomain::Target, true);
const RECORD_ID_ARG: PositionalDef =
    positional(Slot::RecordId, "ID", ValueDomain::RecordId, false);
const ROOT_ARG: PositionalDef =
    positional(Slot::RootDir, "ROOT", ValueDomain::Text("ROOT"), false);
const SIZE_ARG: PositionalDef =
    positional(Slot::Size, "SIZE", ValueDomain::ResizeSize, false);
const OPTIONAL_BYTES_ARG: PositionalDef =
    positional(Slot::Size, "BYTES", ValueDomain::AbsoluteSize, true);
const PROPERTY_ARG: PositionalDef =
    positional(Slot::Property, "PROPERTY", ValueDomain::Text("PROPERTY"), false);
const VALUE_ARG: PositionalDef =
    positional(Slot::Value, "VALUE", ValueDomain::Text("VALUE"), false);
const OPTIONAL_VALUE_ARG: PositionalDef =
    positional(Slot::Value, "VALUE", ValueDomain::Text("VALUE"), true);
const STATE_ARG: PositionalDef =
    positional(Slot::State, "STATE", ValueDomain::Text("on|off"), false);
const FS_TYPE_ARG: PositionalDef =
    positional(Slot::FsType, "FS", ValueDomain::Text("FS"), false);

/// 命令与撤销窗口的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetMode {
    ReadOnly,
    WriteNoJournal,
    WriteJournal,
}

/// 一条命令会造成的副作用类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    ReadOnly,
    TargetData,
    CompanionFile,
    ExternalTool,
    DeviceState,
}

/// 确认门。`Required` 的命令在解析期必须拿到 `--yes`，缺了就是用法错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confirmation {
    /// 解析期必须有 `--yes`
    Required,
    /// 不接受 `--yes`
    None,
}

/// 命令在顶层帮助里的分组
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandGroup {
    Everyday,
    LowLevel,
}

/// 命令声明的一个选项：命令白名单与帮助里的选项块读同一条。
///
/// `desc` 为 `None` 时用旗标注册表里的说明；命令有额外语义时在这里补充，
/// 不另写一段完整说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OptionDef {
    pub(crate) name: &'static str,
    pub(crate) desc: Option<&'static str>,
}

const fn opt(name: &'static str) -> OptionDef {
    OptionDef { name, desc: None }
}

const fn opt_desc(name: &'static str, desc: &'static str) -> OptionDef {
    OptionDef { name, desc: Some(desc) }
}

/// 形态判定项：某个旗标出现，或某个位置参数等于字面量（`set ... name`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selector {
    Flag(&'static str),
    Positional { index: usize, literal: &'static str },
}

/// 一个形态对目标后缀的要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetSuffix {
    /// 带不带 `:N` 都可以
    Any,
    /// 必须不带 `:N`
    Whole,
    /// 必须带 `:N`（或 `:last`）
    Partition,
}

/// 一个命令的一种调用形态。语法、位置参数、选项与判定条件只在这里声明一次：
/// 解析器按它选形态并校验，帮助与参考文档也由它生成。
pub(crate) struct Variant {
    /// `diskedit {name} ` 之后的部分
    pub(crate) syntax: &'static str,
    pub(crate) positionals: &'static [PositionalDef],
    pub(crate) options: &'static [OptionDef],
    /// 全部成立才命中；空表示"没有判别项"
    pub(crate) selects: &'static [Selector],
    pub(crate) target: TargetSuffix,
    /// 无判别项命中时采用这一形态；每个命令至多一个
    pub(crate) default: bool,
}

const fn variant(
    syntax: &'static str,
    positionals: &'static [PositionalDef],
    options: &'static [OptionDef],
    selects: &'static [Selector],
    target: TargetSuffix,
    default: bool,
) -> Variant {
    Variant { syntax, positionals, options, selects, target, default }
}

/// 一条命令的全部声明：名字、分组、形态表、命令级公共选项、详助正文、
/// 确认规则、能力需求与撤销窗口。
///
/// 帮助、参考文档与能力矩阵都从这一份声明生成；解析器读的也是同一份。
pub(crate) struct CommandDef {
    pub(crate) name: &'static str,
    pub(crate) group: CommandGroup,
    /// 顶层帮助里紧跟用法之后的一行
    pub(crate) summary: &'static str,
    /// 形态表：一种调用方式一条
    pub(crate) variants: &'static [Variant],
    /// 每个形态都接受的选项
    pub(crate) common_options: &'static [OptionDef],
    /// 详助正文：用法行与选项块之外的全部内容，原样渲染
    pub(crate) details: &'static str,
    pub(crate) mode: TargetMode,
    pub(crate) capabilities: &'static [Capability],
    pub(crate) confirmation: Confirmation,
    build: fn(&Positionals, Options) -> Request,
}

impl CommandDef {
    /// 该命令是否声明了这个选项（任一形态或公共选项）；拒绝路径与帮助对账读同一份白名单
    pub(crate) fn accepts(&self, flag: &str) -> bool {
        self.common_options.iter().any(|o| o.name == flag)
            || self.variants.iter().any(|v| v.options.iter().any(|o| o.name == flag))
    }

    /// 单个形态的白名单：形态自己的选项加命令级公共选项
    pub(crate) fn accepts_in(&self, v: &Variant, flag: &str) -> bool {
        self.common_options.iter().any(|o| o.name == flag)
            || v.options.iter().any(|o| o.name == flag)
    }

    /// 该命令是否声明了这个槽位
    #[cfg(test)]
    pub(crate) fn has_slot(&self, slot: Slot) -> bool {
        self.variants.iter().any(|v| v.positionals.iter().any(|p| p.slot == slot))
    }

    /// 全部用法行，按声明顺序
    pub(crate) fn syntax_lines(&self) -> impl Iterator<Item = &'static str> {
        self.variants.iter().map(|v| v.syntax)
    }
}

pub(crate) const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "info",
        group: CommandGroup::Everyday,
        summary: "show partition table and per-partition filesystem identification; --grow appends a relocation preview for :N",
        variants: &[
            variant("<TARGET> [--sector-size N]", &[TARGET_ARG], &[], &[], TargetSuffix::Whole, true),
            variant(
                "<TARGET>:N --grow [--sector-size N]",
                &[TARGET_ARG],
                &[opt("--grow")],
                &[Selector::Flag("--grow")],
                TargetSuffix::Partition,
                false,
            ),
        ],
        common_options: &[opt("--sector-size")],
        details: crate::cmd::info::DETAILS,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly],
        confirmation: Confirmation::None,
        build: build_info,
    },
    CommandDef {
        name: "inspect",
        group: CommandGroup::Everyday,
        summary: "show the recovery record beside a target (read-only)",
        variants: &[variant("<TARGET|PATH>", &[TARGET_ARG], &[], &[], TargetSuffix::Whole, true)],
        common_options: &[],
        details: crate::cmd::inspect::DETAILS,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly, Capability::CompanionFile],
        confirmation: Confirmation::None,
        build: build_inspect,
    },
    CommandDef {
        name: "records list",
        group: CommandGroup::Everyday,
        summary: "list recovery records as JSON (omit TARGET: everything under <state_root>)",
        variants: &[variant("[TARGET]", &[OPTIONAL_TARGET_ARG], &[], &[], TargetSuffix::Whole, true)],
        common_options: &[],
        details: crate::cmd::records::DETAILS_LIST,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly, Capability::CompanionFile],
        confirmation: Confirmation::None,
        build: build_records_list,
    },
    CommandDef {
        name: "records verify",
        group: CommandGroup::Everyday,
        summary: "re-check one record by its 32-hex id",
        variants: &[variant(
            "<TARGET> <ID>",
            &[TARGET_ARG, RECORD_ID_ARG],
            &[],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &[],
        details: crate::cmd::records::DETAILS_VERIFY,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly, Capability::CompanionFile],
        confirmation: Confirmation::None,
        build: build_records_verify,
    },
    CommandDef {
        name: "records scan",
        group: CommandGroup::Everyday,
        summary: "enumerate records under a container root (<state_root>/recovery, or an image directory)",
        variants: &[variant("<ROOT>", &[ROOT_ARG], &[], &[], TargetSuffix::Any, true)],
        common_options: &[],
        details: crate::cmd::records::DETAILS_SCAN,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly, Capability::CompanionFile],
        confirmation: Confirmation::None,
        build: build_records_scan,
    },
    CommandDef {
        name: "resize",
        group: CommandGroup::Everyday,
        summary: "resize a partition and its filesystem; --end / --grow-to-end repartition without moving data",
        variants: &[
            variant(
                "<TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes",
                &[TARGET_ARG, SIZE_ARG],
                &[
                    opt("--allow-move"),
                    opt("--chunk"),
                    opt("--dry-run"),
                    opt("--grow-lv"),
                    opt("--lv"),
                ],
                &[],
                TargetSuffix::Any,
                true,
            ),
            variant(
                "<TARGET>:N --end LBA [--align A] [--no-fs] --yes",
                &[TARGET_ARG],
                &[opt("--dry-run"), opt("--end"), opt("--align")],
                &[Selector::Flag("--end")],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N --grow-to-end [--align A] [--no-fs] --yes",
                &[TARGET_ARG],
                &[opt("--dry-run"), opt("--grow-to-end"), opt("--align")],
                &[Selector::Flag("--grow-to-end")],
                TargetSuffix::Partition,
                false,
            ),
        ],
        common_options: &[
            opt_desc(
                "--no-fs",
                "skip the filesystem step of the grown partition; required when the filesystem inside cannot be grown here (limits are under Constraints)",
            ),
            opt("--sector-size"),
            opt("--yes"),
        ],
        details: crate::cmd::resize::DETAILS,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        confirmation: Confirmation::Required,
        build: build_resize,
    },
    CommandDef {
        name: "move",
        group: CommandGroup::Everyday,
        summary: "move a partition, data follows (chunked copy, resumable)",
        variants: &[variant(
            "<TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]",
            &[TARGET_ARG],
            &[
                opt("--align"),
                opt_desc("--chunk", "default 4 MiB; a resumed job keeps the record's value"),
                opt("--start"),
            ],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::layout::DETAILS_MOVE,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        confirmation: Confirmation::Required,
        build: build_move,
    },
    CommandDef {
        name: "copy",
        group: CommandGroup::Everyday,
        summary: "copy a partition to a new location; the source is untouched",
        variants: &[variant(
            "<TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]",
            &[TARGET_ARG],
            &[
                opt("--align"),
                opt_desc("--chunk", "default 4 MiB; a resumed job keeps the record's value"),
                opt("--keep-partuuid"),
                opt("--name"),
                opt("--start"),
            ],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::layout::DETAILS_COPY,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        confirmation: Confirmation::Required,
        build: build_copy,
    },
    CommandDef {
        name: "delete",
        group: CommandGroup::Everyday,
        summary: "delete a partition entry (the data area is not wiped)",
        variants: &[variant(
            "<TARGET>:N --yes",
            &[TARGET_ARG],
            &[],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::layout::DETAILS_DELETE,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        confirmation: Confirmation::Required,
        build: build_delete,
    },
    CommandDef {
        name: "set",
        group: CommandGroup::Everyday,
        summary: "set a partition property (name / label / uuid / flag / type)",
        variants: &[
            variant(
                "<TARGET>:N name S --yes",
                &[TARGET_ARG, PROPERTY_ARG, VALUE_ARG],
                &[],
                &[Selector::Positional { index: 1, literal: "name" }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N label S --yes",
                &[TARGET_ARG, PROPERTY_ARG, VALUE_ARG],
                &[],
                &[Selector::Positional { index: 1, literal: "label" }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N uuid [U] --yes [--random]",
                &[TARGET_ARG, PROPERTY_ARG, OPTIONAL_VALUE_ARG],
                &[],
                &[Selector::Positional { index: 1, literal: "uuid" }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N flag F on|off --yes",
                &[TARGET_ARG, PROPERTY_ARG, VALUE_ARG, STATE_ARG],
                &[],
                &[Selector::Positional { index: 1, literal: "flag" }],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<TARGET>:N type GUID|0xXX --yes",
                &[TARGET_ARG, PROPERTY_ARG, VALUE_ARG],
                &[],
                &[Selector::Positional { index: 1, literal: "type" }],
                TargetSuffix::Partition,
                false,
            ),
        ],
        common_options: &[opt("--random"), opt("--sector-size"), opt("--yes")],
        details: crate::cmd::fs::DETAILS_SET,
        mode: TargetMode::WriteJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        confirmation: Confirmation::Required,
        build: build_set,
    },
    CommandDef {
        name: "check",
        group: CommandGroup::Everyday,
        summary: "check filesystem consistency (no repair; f2fs writes metadata on a SIT mismatch)",
        variants: &[variant(
            "<TARGET>:N",
            &[TARGET_ARG],
            &[],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &[opt("--sector-size")],
        details: crate::cmd::fs::DETAILS_CHECK,
        mode: TargetMode::ReadOnly,
        capabilities: &[Capability::ReadOnly, Capability::ExternalTool],
        confirmation: Confirmation::None,
        build: build_check,
    },
    CommandDef {
        name: "mkfs",
        group: CommandGroup::Everyday,
        summary: "create a filesystem (destroys all data on the partition)",
        variants: &[variant(
            "<TARGET>:N <FS> --yes [--label S]",
            &[TARGET_ARG, FS_TYPE_ARG],
            &[opt("--label")],
            &[],
            TargetSuffix::Partition,
            true,
        )],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::fs::DETAILS_MKFS,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::CompanionFile, Capability::ExternalTool, Capability::DeviceState],
        confirmation: Confirmation::Required,
        build: build_mkfs,
    },
    CommandDef {
        name: "resizefs",
        group: CommandGroup::Everyday,
        summary: "resize a filesystem only — the partition entry stays put",
        variants: &[
            variant(
                "<TARGET>:N [BYTES] --yes",
                &[TARGET_ARG, OPTIONAL_BYTES_ARG],
                &[],
                &[],
                TargetSuffix::Partition,
                false,
            ),
            variant(
                "<MOUNTPOINT> [BYTES] --yes",
                &[TARGET_ARG, OPTIONAL_BYTES_ARG],
                &[],
                &[],
                TargetSuffix::Whole,
                false,
            ),
        ],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::fs::DETAILS_RESIZEFS,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::CompanionFile, Capability::ExternalTool, Capability::DeviceState],
        confirmation: Confirmation::Required,
        build: build_resizefs,
    },
    CommandDef {
        name: "recover",
        group: CommandGroup::Everyday,
        summary: "finish with a recovery record: replay it, or release it without rollback",
        variants: &[
            variant("<TARGET> --yes", &[TARGET_ARG], &[], &[], TargetSuffix::Whole, true),
            variant(
                "<TARGET> --replay-table-only --yes",
                &[TARGET_ARG],
                &[opt("--replay-table-only")],
                &[Selector::Flag("--replay-table-only")],
                TargetSuffix::Whole,
                false,
            ),
            variant(
                "<TARGET> --release [--discard-replay] --yes",
                &[TARGET_ARG],
                &[opt("--release"), opt("--discard-replay")],
                &[Selector::Flag("--release")],
                TargetSuffix::Whole,
                false,
            ),
        ],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::recover::DETAILS,
        mode: TargetMode::WriteNoJournal,
        capabilities: &[
            Capability::TargetData,
            Capability::CompanionFile,
            Capability::ExternalTool,
            Capability::DeviceState,
        ],
        confirmation: Confirmation::Required,
        build: build_recover,
    },
    CommandDef {
        name: "new",
        group: CommandGroup::LowLevel,
        summary: "create a fresh partition table",
        variants: &[variant(
            "<TARGET> [--table gpt|mbr] --yes [--overwrite-data]",
            &[TARGET_ARG],
            &[opt("--table"), opt("--overwrite-data")],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::layout::DETAILS_NEW,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        confirmation: Confirmation::Required,
        build: build_new,
    },
    CommandDef {
        name: "add",
        group: CommandGroup::LowLevel,
        summary: "add a partition; --start fixes where it begins",
        variants: &[variant(
            "<TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes",
            &[TARGET_ARG, SIZE_ARG],
            &[
                opt("--align"),
                opt("--name"),
                opt("--part-number"),
                opt_desc("--start", "starting LBA; SIZE fixes the end"),
                opt("--type-guid"),
            ],
            &[],
            TargetSuffix::Whole,
            true,
        )],
        common_options: &[opt("--sector-size"), opt("--yes")],
        details: crate::cmd::layout::DETAILS_ADD,
        mode: TargetMode::WriteJournal,
        capabilities: &[Capability::TargetData, Capability::CompanionFile, Capability::DeviceState],
        confirmation: Confirmation::Required,
        build: build_add,
    },
];

pub(crate) fn find(name: &str) -> Option<&'static CommandDef> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// 目标路径、分区选择器与扇区大小覆盖。事务层只消费这三项。
pub(crate) trait TargetRequest {
    fn target(&self) -> &str;
    fn sector_size(&self) -> Option<SectorSize>;
}

pub(crate) trait ChunkRequest {
    fn chunk_mib(&self) -> Option<u64>;
}

macro_rules! impl_target_request {
    ($t:ty) => {
        impl TargetRequest for $t {
            fn target(&self) -> &str { &self.target }
            fn sector_size(&self) -> Option<SectorSize> { self.sector_size }
        }
    };
}

macro_rules! impl_chunk_request {
    ($t:ty) => {
        impl ChunkRequest for $t {
            fn chunk_mib(&self) -> Option<u64> { self.chunk_mib }
        }
    };
}

pub(crate) struct InfoRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
}

/// `inspect` 只接受整盘目标，且不提供扇区大小覆盖
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // 解析仍校验这两项，消费方只在 Linux
pub(crate) struct InspectRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
}

/// `records list`：目标可选，省略时列主机仓库里的全部记录
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct RecordsListRequest {
    pub(crate) target: Option<String>,
}

/// `records verify`：按记录编号定位唯一一条，并对目标重新探测身份
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct RecordsVerifyRequest {
    pub(crate) id: String,
    pub(crate) target: String,
}

/// `records scan`：容器根目录，块设备侧是 `<state_root>/recovery`，镜像侧是镜像所在目录
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct RecordsScanRequest {
    pub(crate) root: String,
}

pub(crate) struct ResizeRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) size: SizeSpec,
    pub(crate) no_fs: bool,
    pub(crate) grow_lv: bool,
    pub(crate) lv: Option<String>,
    pub(crate) allow_move: bool,
    /// `--chunk` 的显式值；`None` 表示未给出（续跑取记录值，新作业取默认值）
    pub(crate) chunk_mib: Option<u64>,
    pub(crate) dry_run: bool,
}

pub(crate) struct MoveRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) start: Option<StartSelector>,
    pub(crate) align: Align,
    pub(crate) align_given: bool,
    pub(crate) chunk_mib: Option<u64>,
}

pub(crate) struct CopyRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) start: Option<StartSelector>,
    pub(crate) align: Align,
    pub(crate) align_given: bool,
    pub(crate) chunk_mib: Option<u64>,
    pub(crate) name: Option<String>,
    pub(crate) keep_partuuid: bool,
}

pub(crate) struct DeleteRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
}

pub(crate) struct SetRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) state: String,
    pub(crate) random: bool,
}

pub(crate) struct CheckRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
}

pub(crate) struct MkfsRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) fstype: String,
    pub(crate) label: Option<String>,
}

pub(crate) struct ResizefsRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    /// 在线形式的绝对字节数；离线形式恒为 None
    pub(crate) bytes: Option<u64>,
    /// 位置参数原样保留，供离线形式拒绝时报出用户写下的值
    pub(crate) bytes_text: Option<String>,
}

pub(crate) struct NewRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) table: Option<crate::table::TableKind>,
    pub(crate) overwrite_data: bool,
}

pub(crate) struct AddRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    /// 要完整容纳的分区尺寸
    pub(crate) size: SizeSpec,
    /// 起点；省略时取首个能容纳 `size` 的空洞
    pub(crate) start: Option<StartSelector>,
    pub(crate) align: Align,
    /// `--align` 是否被显式给出；自动放置的缺省对齐与显式 `--align none` 由此分开
    pub(crate) align_given: bool,
    pub(crate) part_number: Option<u32>,
    pub(crate) name: Option<String>,
    pub(crate) type_guid: Option<TypeSelector>,
}

pub(crate) struct ResizeEndRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) end: Option<u64>,
    pub(crate) grow_to_end: bool,
    pub(crate) align: Align,
    pub(crate) no_fs: bool,
    pub(crate) dry_run: bool,
}

/// `recover`：一个命令、两种归宿。`release` 为真时不回放任何字节，只释放记录文件；
/// 否则回放记录，`replay_table_only` 允许跳过已搬走的区间
pub(crate) struct RecoverRequest {
    pub(crate) target: String,
    pub(crate) part: Option<PartSelector>,
    pub(crate) sector_size: Option<SectorSize>,
    pub(crate) release: bool,
    pub(crate) discard_replay: bool,
    pub(crate) replay_table_only: bool,
}

pub(crate) enum Request {
    Info(InfoRequest),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // 解析仍构造该请求，消费方只在 Linux
    Inspect(InspectRequest),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    RecordsList(RecordsListRequest),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    RecordsVerify(RecordsVerifyRequest),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    RecordsScan(RecordsScanRequest),
    Resize(ResizeRequest),
    Move(MoveRequest),
    Copy(CopyRequest),
    Delete(DeleteRequest),
    Set(SetRequest),
    Check(CheckRequest),
    Mkfs(MkfsRequest),
    Resizefs(ResizefsRequest),
    Recover(RecoverRequest),
    New(NewRequest),
    Add(AddRequest),
    ResizeEnd(ResizeEndRequest),
    /// 只读报告，与 `info` 同一份请求
    InfoGrow(InfoRequest),
}

impl Request {
    pub(crate) fn def(&self) -> &'static CommandDef {
        let name = match self {
            Self::Info(_) => "info",
            Self::Inspect(_) => "inspect",
            Self::RecordsList(_) => "records list",
            Self::RecordsVerify(_) => "records verify",
            Self::RecordsScan(_) => "records scan",
            Self::Resize(_) => "resize",
            Self::Move(_) => "move",
            Self::Copy(_) => "copy",
            Self::Delete(_) => "delete",
            Self::Set(_) => "set",
            Self::Check(_) => "check",
            Self::Mkfs(_) => "mkfs",
            Self::Resizefs(_) => "resizefs",
            Self::Recover(_) => "recover",
            Self::New(_) => "new",
            Self::Add(_) => "add",
            Self::ResizeEnd(_) => "resize",
            Self::InfoGrow(_) => "info",
        };
        find(name).expect("request variant has a command definition")
    }
}

impl_target_request!(InfoRequest);
impl_target_request!(ResizeRequest);
impl_target_request!(MoveRequest);
impl_target_request!(CopyRequest);
impl_target_request!(DeleteRequest);
impl_target_request!(SetRequest);
impl_target_request!(CheckRequest);
impl_target_request!(MkfsRequest);
impl_target_request!(ResizefsRequest);
impl_target_request!(RecoverRequest);
impl_target_request!(NewRequest);
impl_target_request!(AddRequest);
impl_target_request!(ResizeEndRequest);

impl_chunk_request!(ResizeRequest);
impl_chunk_request!(MoveRequest);
impl_chunk_request!(CopyRequest);

/// 位置参数按已选形态的槽位读取
struct Positionals<'a> {
    def: &'static CommandDef,
    variant: &'static Variant,
    tokens: &'a [String],
}

impl Positionals<'_> {
    fn index(&self, slot: Slot) -> Option<usize> {
        self.variant.positionals.iter().position(|p| p.slot == slot)
    }

    fn get(&self, slot: Slot) -> Option<&str> {
        self.index(slot).and_then(|i| self.tokens.get(i)).map(String::as_str)
    }

    /// 按该槽位声明的值域解析；值域不合法是用法错误（2）
    fn parsed(&self, slot: Slot) -> Option<Parsed> {
        let i = self.index(slot)?;
        let text = self.tokens.get(i)?;
        let def = &self.variant.positionals[i];
        Some(def.domain.parse(text).unwrap_or_else(|reason| {
            bail_fail(RunOutcome::usage(format!(
                "bad {} {text:?} ({reason}; see `diskedit help {}`)",
                def.name, self.def.name
            )))
        }))
    }
}

/// 解析后的旗标取值。命令层看不到它；各命令的 `build` 把它转成 [`Request`]
struct Options {
    yes: bool,
    overwrite_data: bool,
    random: bool,
    keep_partuuid: bool,
    sector_size: Option<SectorSize>,
    align: Align,
    align_given: bool,
    chunk_mib: Option<u64>,
    grow_to_end: bool,
    grow: bool,
    dry_run: bool,
    discard_replay: bool,
    replay_table_only: bool,
    release: bool,
    allow_move: bool,
    no_fs: bool,
    grow_lv: bool,
    lv: Option<String>,
    table: Option<crate::table::TableKind>,
    start: Option<StartSelector>,
    end: Option<u64>,
    name: Option<String>,
    type_guid: Option<TypeSelector>,
    label: Option<String>,
    part_number: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            yes: false,
            overwrite_data: false,
            random: false,
            keep_partuuid: false,
            sector_size: None,
            align: Align::none(),
            align_given: false,
            chunk_mib: None,
            grow_to_end: false,
            grow: false,
            dry_run: false,
            discard_replay: false,
            replay_table_only: false,
            release: false,
            allow_move: false,
            no_fs: false,
            grow_lv: false,
            lv: None,
            table: None,
            start: None,
            end: None,
            name: None,
            type_guid: None,
            label: None,
            part_number: None,
        }
    }
}

/// 目标与 `:N`。所有 builder 先取它，再按命令语义补齐其余字段。
fn target_of(p: &Positionals) -> (String, Option<PartSelector>) {
    match p.parsed(Slot::Target) {
        Some(Parsed::Target { target, part }) => (target, part),
        Some(other) => bail_fail(RunOutcome::internal(format!("TARGET slot produced {other:?}"))),
        None => bail_fail(RunOutcome::usage(format!(
            "`{}` needs a target (see diskedit help {})",
            p.def.name, p.def.name
        ))),
    }
}

/// `Text` 值域的槽位。缺省与其它变体都会在命令层被拒绝，这里只做类型收敛。
fn text_of(p: &Positionals, slot: Slot) -> Option<String> {
    match p.parsed(slot)? {
        Parsed::Text(text) => Some(text),
        other => bail_fail(RunOutcome::internal(format!("{slot:?} slot produced {other:?}"))),
    }
}

fn require_part(part: Option<PartSelector>, message: &str) -> Option<PartSelector> {
    if part.is_none() {
        bail_fail(RunOutcome::usage(message.to_string()));
    }
    part
}

/// `--type-guid` 的值域：GPT 用标准文本形 8-4-4-4-12（输入接受
/// 大写），MBR 用 `0xNN` 两位十六进制；其余写法不在值域内
fn parse_type_guid(text: &str) -> Result<TypeSelector, String> {
    const SHAPE: &str =
        "a GUID in 8-4-4-4-12 form (e.g. 0fc63daf-8483-4772-8e79-3d69d8477de4) or an MBR type byte 0xNN";
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        if hex.len() == 2
            // audit-allow: S2-19
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            return Ok(TypeSelector::Mbr(byte));
        }
        return Err(SHAPE.to_string());
    }
    if text.len() != 36 {
        return Err(SHAPE.to_string());
    }
    parse_guid(text).map(TypeSelector::Guid).ok_or_else(|| SHAPE.to_string())
}

fn build_info(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    if o.grow {
        return Request::InfoGrow(InfoRequest { target, part, sector_size: o.sector_size });
    }
    Request::Info(InfoRequest { target, part, sector_size: o.sector_size })
}

fn build_inspect(p: &Positionals, _o: Options) -> Request {
    let (target, part) = target_of(p);
    if part.is_some() {
        bail_fail(RunOutcome::usage(format!(
            "`inspect` takes a whole target, not a partition — drop the :N suffix (got {target})"
        )));
    }
    Request::Inspect(InspectRequest { target, part })
}

fn build_records_list(p: &Positionals, _o: Options) -> Request {
    let target = match p.parsed(Slot::OptionalTarget) {
        None => None,
        Some(Parsed::Target { target, part }) => {
            if part.is_some() {
                bail_fail(RunOutcome::usage(format!(
                    "`records list` takes a whole target, not a partition — drop the :N suffix (got {target})"
                )));
            }
            Some(target)
        }
        Some(other) => bail_fail(RunOutcome::internal(format!("optional TARGET slot produced {other:?}"))),
    };
    Request::RecordsList(RecordsListRequest { target })
}

fn build_records_verify(p: &Positionals, _o: Options) -> Request {
    let id = match p.parsed(Slot::RecordId) {
        Some(Parsed::Text(id)) => id,
        Some(other) => bail_fail(RunOutcome::internal(format!("ID slot produced {other:?}"))),
        None => bail_fail(RunOutcome::usage("`records verify` needs a record ID".to_string())),
    };
    let (target, part) = target_of(p);
    if part.is_some() {
        bail_fail(RunOutcome::usage(format!(
            "`records verify` takes a whole target, not a partition — drop the :N suffix (got {target})"
        )));
    }
    Request::RecordsVerify(RecordsVerifyRequest { id, target })
}

fn build_records_scan(p: &Positionals, _o: Options) -> Request {
    let root = match p.parsed(Slot::RootDir) {
        Some(Parsed::Text(root)) => root,
        Some(other) => bail_fail(RunOutcome::internal(format!("ROOT slot produced {other:?}"))),
        None => bail_fail(RunOutcome::usage("`records scan` needs a root directory".to_string())),
    };
    Request::RecordsScan(RecordsScanRequest { root })
}

fn build_resize(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    // `--end` / `--grow-to-end` 是原地重划形态：没有 SIZE，走分区层重划
    if o.end.is_some() || o.grow_to_end {
        return Request::ResizeEnd(ResizeEndRequest {
            target,
            part,
            sector_size: o.sector_size,
            end: o.end,
            grow_to_end: o.grow_to_end,
            align: o.align,
            no_fs: o.no_fs,
            dry_run: o.dry_run,
        });
    }
    let size = match p.parsed(Slot::Size) {
        Some(Parsed::ResizeSize(size)) => size,
        Some(other) => bail_fail(RunOutcome::internal(format!("SIZE slot produced {other:?}"))),
        None => bail_fail(RunOutcome::usage(
            "specify a SIZE (e.g. +20G, -500M, 10G, grow; see diskedit help resize)".to_string(),
        )),
    };
    Request::Resize(ResizeRequest {
        target,
        part,
        sector_size: o.sector_size,
        size,
        no_fs: o.no_fs,
        grow_lv: o.grow_lv,
        lv: o.lv,
        allow_move: o.allow_move,
        chunk_mib: o.chunk_mib,
        dry_run: o.dry_run,
    })
}

fn build_move(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let part = require_part(part, "`move` needs a partition: write <TARGET>:N (see diskedit help move)");
    let start = o.start.unwrap_or_else(|| {
        bail_fail(RunOutcome::usage(
            "`move` needs --start (an LBA, or `end` for pack-to-end) — see diskedit help move".to_string(),
        ))
    });
    Request::Move(MoveRequest {
        target,
        part,
        sector_size: o.sector_size,
        start: Some(start),
        align: o.align,
        align_given: o.align_given,
        chunk_mib: o.chunk_mib,
    })
}

fn build_copy(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let part = require_part(part, "`copy` needs a partition: write <TARGET>:N (see diskedit help copy)");
    let start = o.start.unwrap_or_else(|| {
        bail_fail(RunOutcome::usage(
            "`copy` needs --start (an LBA, or `end` for pack-to-end) — see diskedit help copy".to_string(),
        ))
    });
    Request::Copy(CopyRequest {
        target,
        part,
        sector_size: o.sector_size,
        start: Some(start),
        align: o.align,
        align_given: o.align_given,
        chunk_mib: o.chunk_mib,
        name: o.name,
        keep_partuuid: o.keep_partuuid,
    })
}

fn build_delete(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let part = require_part(part, "`delete` needs a partition: write <TARGET>:N (see diskedit help delete)");
    Request::Delete(DeleteRequest { target, part, sector_size: o.sector_size })
}

fn build_set(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let part = require_part(part, "`set` needs a partition: write <TARGET>:N (see diskedit help set)");
    let key = text_of(p, Slot::Property).unwrap_or_else(|| {
        bail_fail(RunOutcome::internal(
            "`set` reached the builder without a property slot".to_string(),
        ))
    });
    let value = text_of(p, Slot::Value).unwrap_or_default();
    // `uuid` 的取值方式二选一：显式值与 `--random` 同时出现只看 argv 就能判定，
    // 不必等到打开目标、取锁之后才拒绝
    if o.random && key == "uuid" && !value.is_empty() {
        bail_fail(RunOutcome::usage("`uuid` takes either a value or --random, not both".to_string()));
    }
    Request::Set(SetRequest {
        target,
        part,
        sector_size: o.sector_size,
        key,
        value,
        state: text_of(p, Slot::State).unwrap_or_default(),
        random: o.random,
    })
}

fn build_check(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let part = require_part(part, "`check` needs a partition: write <TARGET>:N (see diskedit help check)");
    Request::Check(CheckRequest { target, part, sector_size: o.sector_size })
}

fn build_mkfs(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let part = require_part(part, "`mkfs` needs a partition: write <TARGET>:N (see diskedit help mkfs)");
    let fstype = text_of(p, Slot::FsType).unwrap_or_else(|| {
        bail_fail(RunOutcome::usage(
            "`mkfs` needs a filesystem type: diskedit mkfs <TARGET>:N <FS> (see diskedit help mkfs)"
                .to_string(),
        ))
    });
    Request::Mkfs(MkfsRequest {
        target,
        part,
        sector_size: o.sector_size,
        fstype,
        label: o.label,
    })
}

fn build_resizefs(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let text = p.get(Slot::Size);
    // 挂载点形式把 BYTES 按值域解析成绝对字节；离线形式把它原样交给命令层拒绝
    let bytes = match (text, part) {
        (Some(_), None) => Some(match p.parsed(Slot::Size) {
            Some(Parsed::Absolute(bytes)) => bytes,
            Some(other) => bail_fail(RunOutcome::internal(format!("BYTES slot produced {other:?}"))),
            None => bail_fail(RunOutcome::internal("BYTES slot has no token".to_string())),
        }),
        _ => None,
    };
    Request::Resizefs(ResizefsRequest {
        target,
        part,
        sector_size: o.sector_size,
        bytes,
        bytes_text: text.map(str::to_string),
    })
}

fn build_recover(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    Request::Recover(RecoverRequest {
        target,
        part,
        sector_size: o.sector_size,
        release: o.release,
        discard_replay: o.discard_replay,
        replay_table_only: o.replay_table_only,
    })
}

fn build_new(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    Request::New(NewRequest {
        target,
        part,
        sector_size: o.sector_size,
        table: o.table,
        overwrite_data: o.overwrite_data,
    })
}

fn build_add(p: &Positionals, o: Options) -> Request {
    let (target, part) = target_of(p);
    let size = match p.parsed(Slot::Size) {
        Some(Parsed::ResizeSize(size @ SizeSpec::Absolute(_))) => size,
        Some(Parsed::ResizeSize(_)) => bail_fail(RunOutcome::usage(
            "`add` needs an absolute SIZE, e.g. 512M (see diskedit help add)".to_string(),
        )),
        Some(other) => bail_fail(RunOutcome::internal(format!("SIZE slot produced {other:?}"))),
        None => bail_fail(RunOutcome::usage(
            "`add` needs a SIZE: diskedit add <TARGET> SIZE (see diskedit help add)".to_string(),
        )),
    };
    Request::Add(AddRequest {
        target,
        part,
        sector_size: o.sector_size,
        size,
        start: o.start,
        align: o.align,
        align_given: o.align_given,
        part_number: o.part_number,
        name: o.name,
        type_guid: o.type_guid,
    })
}

/// SIZE 槽位：`grow` 或 [`parse_size_delta`] 的结果
fn parse_resize_size(s: &str) -> Result<Parsed, String> {
    if s == "grow" {
        return Ok(Parsed::ResizeSize(SizeSpec::Grow));
    }
    match parse_size_delta(s) {
        Some(SizeArg::Bytes { kind: 0, bytes }) => {
            Ok(Parsed::ResizeSize(SizeSpec::Absolute(bytes)))
        }
        Some(SizeArg::Bytes { kind, bytes }) => {
            Ok(Parsed::ResizeSize(SizeSpec::Delta { grow: kind > 0, bytes }))
        }
        Some(SizeArg::Percent { kind, num, denom }) => {
            Ok(Parsed::ResizeSize(SizeSpec::Percent { grow: kind > 0, num, denom }))
        }
        None => Err("use 10G | 1.5G | +2G | -500M | +10% | grow".to_string()),
    }
}

/// 解析命令行并产出类型化的请求；用法错误直接退出（2）
pub(crate) fn parse() -> (&'static CommandDef, Request) {
    parse_from(std::env::args_os().skip(1))
}

fn parse_from(argv: impl Iterator<Item = std::ffi::OsString>) -> (&'static CommandDef, Request) {
    let mut it = argv.map(|arg| {
        arg.into_string().unwrap_or_else(|bad| {
            bail_fail(RunOutcome::usage(format!(
                "argument {:?} is not valid UTF-8 — diskedit takes its arguments as text",
                bad.to_string_lossy()
            )))
        })
    });
    let mut cmd = it.next().unwrap_or_else(|| usage());
    // `records` 是双词命令：命令表按完整名字登记，分派、帮助与能力门仍只认一个字符串
    if cmd == "records" {
        match it.next() {
            Some(sub) if !sub.starts_with('-') => cmd = format!("records {sub}"),
            _ => bail_fail(RunOutcome::usage(
                "`records` needs a subcommand: `records list`, `records verify`, or `records scan` \
                 (see diskedit help \"records list\")"
                    .to_string(),
            )),
        }
    }
    let def = find(&cmd);
    let mut opts = Options::default();
    let mut positional: Vec<String> = Vec::new();
    let mut seen: Vec<&'static str> = Vec::new();
    let mut help: Option<Option<String>> = None;
    let mut version = matches!(cmd.as_str(), "--version" | "-V");
    let mut end_of_opts = false;
    while let Some(arg) = it.next() {
        if end_of_opts {
            positional.push(arg);
            continue;
        }
        if arg == "--" {
            end_of_opts = true;
            continue;
        }
        let mut inline: Option<String> = None;
        let arg = match arg.split_once('=') {
            Some((flag, v)) if flag.starts_with("--") => {
                inline = Some(v.to_string());
                flag.to_string()
            }
            _ => arg,
        };
        let flag = arg.as_str();
        let is_bare_positional = if let Some(spec) = flag_spec(flag) {
            if spec.domain.takes_value() && seen.contains(&flag) {
                bail_fail(RunOutcome::usage(format!(
                    "{flag} {} is given more than once — a value option cannot take two values",
                    spec.domain.placeholder()
                )));
            }
            seen.push(spec.name);
            let value = if spec.domain.takes_value() {
                let v = take_value(&mut inline, &mut it, spec.name);
                Some(spec.domain.parse(&v).unwrap_or_else(|reason| {
                    bad_arg(spec.name, &v, &format!(" ({reason})"))
                }))
            } else {
                None
            };
            store_flag(&mut opts, spec, value);
            false
        } else if flag == "--version" || flag == "-V" {
            version = true;
            false
        } else if flag == "--help" || flag == "-h" {
            if help.is_none() {
                help = Some(if matches!(cmd.as_str(), "help" | "--help" | "-h") {
                    help_topic(&positional)
                } else {
                    Some(cmd.clone())
                });
            }
            false
        } else if flag.starts_with('-') {
            // 相对尺寸只在声明的 SIZE 槽位放行
            let size_slot = def.is_some_and(|d| {
                d.variants.iter().any(|v| {
                    v.positionals.get(positional.len()).is_some_and(|p| p.slot == Slot::Size)
                })
            });
            if !(size_slot && parse_size_delta(flag).is_some()) {
                bail_fail(RunOutcome::usage(format!(
                    "unknown flag `{flag}` — run `diskedit help` for the accepted flags (a value starting \
                     with `-` is accepted as a relative size, e.g. -10G, or after `--`)"
                )))
            }
            true
        } else {
            true
        };
        if is_bare_positional {
            positional.push(arg);
            continue;
        }
        // 等号右侧的值只归带参旗标消费
        if let Some(v) = inline {
            bail_fail(RunOutcome::usage(format!(
                "{flag} takes no value — write it as a bare flag (got {v:?})"
            )));
        }
    }
    if version && help.is_some() {
        bail_fail(RunOutcome::usage(
            "`--version` and `--help` name two different outputs — ask for one at a time".to_string(),
        ));
    }
    if let Some(topic) = help {
        match topic {
            Some(t) => {
                refuse_unconsumed(&t, &seen);
                help_cmd(&t);
            }
            None => {
                refuse_flags_on_bare_help(&seen);
                usage_help();
            }
        }
    }
    if version {
        if let Some(f) = seen.first() {
            bail_fail(RunOutcome::usage(format!("`--version` takes no other options (got {f})")));
        }
        if let Some(t) = positional.first() {
            bail_fail(RunOutcome::usage(format!("`--version` takes no other arguments (got {t})")));
        }
        version_help();
    }
    if matches!(cmd.as_str(), "help" | "?" | "--help" | "-h") {
        match help_topic(&positional) {
            Some(topic) => {
                refuse_unconsumed(&topic, &seen);
                help_cmd(&topic);
            }
            None => {
                refuse_flags_on_bare_help(&seen);
                usage_help();
            }
        }
    }
    let Some(def) = def else { usage() };
    let chosen = choose_variant(def, &positional, &seen);
    // 多余的位置参数在形态选定后判：可选槽位之后的任何值都没有归属
    if let Some(extra) = positional.get(chosen.positionals.len()) {
        bail_fail(RunOutcome::usage(format!(
            "unexpected extra argument for `{}`: {extra} (see `diskedit help {}`)",
            def.name, def.name
        )));
    }
    for f in &seen {
        if !def.accepts_in(chosen, f) {
            bail_fail(RunOutcome::usage(format!(
                "{f} is not valid for `diskedit {} {}` (see diskedit help {})",
                def.name, chosen.syntax, def.name
            )));
        }
        if let Some(spec) = flag_spec(f) {
            for req in spec.requires {
                if !seen.contains(req) {
                    bail_fail(RunOutcome::usage(format!(
                        "{f} requires {req} (see diskedit help {})",
                        def.name
                    )));
                }
            }
        }
    }
    let confirmed = opts.yes;
    // `--dry-run` 只读盘点：它不写盘，因此不需要确认
    let dry_run = opts.dry_run;
    let built = (def.build)(
        &Positionals { def, variant: chosen, tokens: &positional },
        opts,
    );
    // `Confirmation::Required` 的命令缺 `--yes` 在解析期落 2。
    // 放在 build 之后：取值域与槽位的错误更靠近出错的那一处，先报它们
    if def.confirmation == Confirmation::Required && !confirmed && !dry_run {
        bail_fail(RunOutcome::usage(format!(
            "`{}` needs --yes (see diskedit help {})",
            def.name, def.name
        )));
    }
    (def, built)
}

/// `--help` / `help` 的主题：`records` 后紧跟的子命令并入主题，其余取首词。
/// 主题按完整词序列取，`diskedit help records list` 与 `diskedit -h records list` 等价
fn help_topic(tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    Some(if first == "records" {
        match tokens.get(1) {
            Some(sub) if !sub.starts_with('-') => format!("records {sub}"),
            _ => "records".to_string(),
        }
    } else {
        first.clone()
    })
}

/// 目标 token 是否带分区号；目标槽位缺失或缺值为 `None`（后缀判据不成立，
/// 缺参留给形态的位置参数检查报）
fn target_suffix(def: &CommandDef, positional: &[String]) -> Option<bool> {
    let index = def.variants.first()?.positionals.iter()
        .position(|p| matches!(p.slot, Slot::Target | Slot::OptionalTarget))?;
    let text = positional.get(index)?;
    match ValueDomain::Target.parse(text) {
        Ok(Parsed::Target { part, .. }) => Some(part.is_some()),
        _ => None,
    }
}

fn suffix_ok(v: &Variant, suffix: Option<bool>) -> bool {
    match (v.target, suffix) {
        (TargetSuffix::Any, _) | (_, None) => true,
        (TargetSuffix::Whole, Some(part)) => !part,
        (TargetSuffix::Partition, Some(part)) => part,
    }
}

fn check_suffix(def: &CommandDef, v: &Variant, suffix: Option<bool>) {
    if suffix_ok(v, suffix) {
        return;
    }
    match v.target {
        TargetSuffix::Partition => bail_fail(RunOutcome::usage(format!(
            "`{}` needs a partition: write <TARGET>:N (see `diskedit help {}`)",
            def.name, def.name
        ))),
        TargetSuffix::Whole => bail_fail(RunOutcome::usage(format!(
            "`{} {}` takes a whole target, not a partition — drop the :N suffix (see `diskedit help {}`)",
            def.name, v.syntax, def.name
        ))),
        TargetSuffix::Any => {}
    }
}

/// 位置参数判别项（`set ... name`）：返回 (下标, 该槽位的声明) 与全部候选字面量。
/// 用于"一个判别项都没命中"时的缺参/取值文案
fn positional_selectors(def: &CommandDef) -> Option<(usize, &'static PositionalDef, Vec<&'static str>)> {
    let mut found: Option<(usize, &'static PositionalDef)> = None;
    let mut literals = Vec::new();
    for v in def.variants {
        for s in v.selects {
            if let Selector::Positional { index, literal } = s {
                literals.push(*literal);
                if found.is_none() {
                    found = v.positionals.get(*index).map(|p| (*index, p));
                }
            }
        }
    }
    found.map(|(index, p)| (index, p, literals))
}

/// 按声明选形态：先看判别项（旗标/位置参数字面量），再用目标后缀过滤；
/// 无判别项命中时用标了 `default` 的形态。≥2 命中按用法错误拒绝
fn choose_variant(
    def: &'static CommandDef,
    positional: &[String],
    seen: &[&'static str],
) -> &'static Variant {
    let suffix = target_suffix(def, positional);
    let holds = |v: &Variant| {
        v.selects.iter().all(|s| match s {
            Selector::Flag(f) => seen.contains(f),
            Selector::Positional { index, literal } => {
                positional.get(*index).map(String::as_str) == Some(*literal)
            }
        })
    };
    let selected: Vec<&'static Variant> = def
        .variants
        .iter()
        .filter(|v| !v.selects.is_empty() && holds(v))
        .collect();
    match selected.len() {
        1 => {
            check_suffix(def, selected[0], suffix);
            selected[0]
        }
        n if n > 1 => bail_fail(RunOutcome::usage(format!(
            "`{}` matches more than one form ({}); pick one",
            def.name,
            selected.iter().map(|v| v.syntax).collect::<Vec<_>>().join(" | ")
        ))),
        _ => {
            let fits: Vec<&'static Variant> = def
                .variants
                .iter()
                .filter(|v| v.selects.is_empty() && suffix_ok(v, suffix))
                .collect();
            if fits.len() == 1 {
                return fits[0];
            }
            // 多个空判别项形态同时成立时，采用声明为 default 的那一个
            if let Some(v) = fits.iter().find(|v| v.default) {
                return v;
            }
            if let Some((index, p, literals)) = positional_selectors(def) {
                if positional.len() <= index {
                    bail_fail(RunOutcome::usage(format!(
                        "`{}` needs a {}: {} — see `diskedit help {}`",
                        def.name,
                        p.name.to_ascii_lowercase(),
                        literals.join(", "),
                        def.name
                    )));
                }
                bail_fail(RunOutcome::usage(format!(
                    "`{}` does not accept {} as a {} (expected {}) — see `diskedit help {}`",
                    def.name,
                    positional[index],
                    p.name.to_ascii_lowercase(),
                    literals.join(", "),
                    def.name
                )));
            }
            // 无判别项且后缀把全部形态都排除：用默认形态（或首个空判别项形态）报后缀要求
            if let Some(v) = def
                .variants
                .iter()
                .find(|v| v.default)
                .or_else(|| def.variants.iter().find(|v| v.selects.is_empty()))
            {
                check_suffix(def, v, suffix);
            }
            if positional.is_empty() {
                for v in def.variants {
                    if let Some(p) = v.positionals.first() {
                        bail_fail(RunOutcome::usage(format!(
                            "`{}` needs {} — see `diskedit help {}`",
                            def.name, p.name, def.name
                        )));
                    }
                }
            }
            bail_fail(RunOutcome::usage(format!(
                "`{}` does not match any accepted form — see `diskedit help {}`",
                def.name, def.name
            )))
        }
    }
}

fn refuse_unconsumed(cmd: &str, seen: &[&'static str]) {
    let Some(def) = find(cmd) else { return };
    for f in seen {
        if !def.accepts(f) {
            bail_fail(RunOutcome::usage(format!(
                "{f} is not a valid option for `{cmd}` (see diskedit help {cmd})"
            )));
        }
    }
}

fn refuse_flags_on_bare_help(seen: &[&'static str]) {
    if let Some(f) = seen.first() {
        bail_fail(RunOutcome::usage(format!("{f} is not a valid option for `help` (see diskedit help)")));
    }
}

fn take_value(inline: &mut Option<String>, it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match inline.take() {
        Some(v) => v,
        None => match it.next() {
            Some(v) if !v.starts_with('-') => v,
            _ => miss_arg(flag),
        },
    }
}

fn miss_arg(flag: &str) -> ! {
    let placeholder =
        flag_spec(flag).map(|s| s.domain.placeholder()).unwrap_or("VALUE");
    bail_fail(RunOutcome::usage(format!("{flag} {placeholder} requires a value (see diskedit help)")))
}

fn bad_arg(flag: &str, v: &str, hint: &str) -> ! {
    bail_fail(RunOutcome::usage(format!("bad value {v:?} for {flag}{hint}")))
}

/// 按旗标声明的落点写入请求字段。值域与落点对不上即 schema 自相矛盾，按内部错误终止。
fn store_flag(opts: &mut Options, spec: &FlagSpec, value: Option<Parsed>) {
    match (spec.effect, value) {
        (Effect::Yes, None) => opts.yes = true,
        (Effect::OverwriteData, None) => opts.overwrite_data = true,
        (Effect::Random, None) => opts.random = true,
        (Effect::KeepPartuuid, None) => opts.keep_partuuid = true,
        (Effect::GrowToEnd, None) => opts.grow_to_end = true,
        (Effect::Grow, None) => opts.grow = true,
        (Effect::DryRun, None) => opts.dry_run = true,
        (Effect::DiscardReplay, None) => opts.discard_replay = true,
        (Effect::Release, None) => opts.release = true,
        (Effect::ReplayTableOnly, None) => opts.replay_table_only = true,
        (Effect::AllowMove, None) => opts.allow_move = true,
        (Effect::NoFs, None) => opts.no_fs = true,
        (Effect::GrowLv, None) => opts.grow_lv = true,
        // --lv 的 desc（"implies --grow-lv"）与消费点共用这一承诺：给了 --lv 却不扩
        // LV 的第三种状态不存在。漏了这一步，四处消费点全按 grow_lv 门控读 lv，
        // 单给 --lv 的请求会被静默忽略且报成功
        (Effect::Lv, Some(Parsed::Text(v))) => {
            opts.lv = Some(v);
            opts.grow_lv = true;
        }
        (Effect::Name, Some(Parsed::Text(v))) => opts.name = Some(v),
        (Effect::Label, Some(Parsed::Text(v))) => opts.label = Some(v),
        (Effect::TypeGuid, Some(Parsed::Type(v))) => opts.type_guid = Some(v),
        (Effect::PartNumber, Some(Parsed::PartNumber(v))) => opts.part_number = Some(v),
        (Effect::SectorSize, Some(Parsed::SectorSizeValue(v))) => opts.sector_size = Some(v),
        (Effect::Start, Some(Parsed::Start(v))) => opts.start = Some(v),
        (Effect::End, Some(Parsed::Lba(v))) => opts.end = Some(v),
        (Effect::AlignValue, Some(Parsed::AlignValue(v))) => {
            opts.align = v;
            opts.align_given = true;
        }
        (Effect::ChunkSize, Some(Parsed::Mebibytes(v))) => opts.chunk_mib = Some(v),
        (Effect::Table, Some(Parsed::TableKind(v))) => opts.table = Some(v),
        _ => bail_fail(RunOutcome::internal(format!("{} does not match its value domain", spec.name))),
    }
}

/// SIZE 数值。`Bytes` 已按单位折成整字节（小数向下取整）；`Percent` 保留分子/分母。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SizeArg {
    Bytes { kind: i8, bytes: u64 },
    Percent { kind: i8, num: u64, denom: u64 },
}

/// SIZE 字符串解析。单位 k/m/g/t（1024 进制，大小写均可），无单位 = 字节；
/// 小数按十进制位精确换算；"+10%/-10%" 为锚定当前分区字节数的百分比增量；
/// 绝对百分比 "10%" 有歧义故不受理；"grow" 由调用方先行处理
pub(crate) fn parse_size_delta(s: &str) -> Option<SizeArg> {
    let (kind, rest) = match s.as_bytes().first()? {
        b'+' => (1i8, &s[1..]),
        b'-' => (-1i8, &s[1..]),
        _ => (0i8, s),
    };
    if rest.is_empty() {
        return None;
    }
    if rest.starts_with('+') || rest.starts_with('-') {
        return None;
    }
    if let Some(pct) = rest.strip_suffix('%') {
        if kind == 0 {
            return None;
        }
        let (num, denom) = parse_decimal(pct)?;
        return Some(SizeArg::Percent { kind, num, denom });
    }
    let (mult, text) = match rest.as_bytes().last()? {
        b'k' | b'K' => (1024u64, &rest[..rest.len() - 1]),
        b'm' | b'M' => (1024 * 1024, &rest[..rest.len() - 1]),
        b'g' | b'G' => (1024 * 1024 * 1024, &rest[..rest.len() - 1]),
        b't' | b'T' => (1024u64 * 1024 * 1024 * 1024, &rest[..rest.len() - 1]),
        b'0'..=b'9' | b'.' => (1u64, rest),
        _ => return None,
    };
    let (num, denom) = parse_decimal(text)?;
    let bytes = (num as u128).checked_mul(mult as u128)?.checked_div(denom as u128)?;
    (bytes <= u64::MAX as u128).then_some(SizeArg::Bytes { kind, bytes: bytes as u64 })
}

fn parse_decimal(text: &str) -> Option<(u64, u64)> {
    if text.is_empty() || text.ends_with('.') {
        return None;
    }
    let (int_part, frac_part) = match text.split_once('.') {
        Some((i, f)) => (i, f),
        None => (text, ""),
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if frac_part.len() > 18 {
        return None;
    }
    let denom = 10u64.checked_pow(frac_part.len() as u32)?;
    let int: u64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let frac: u64 = if frac_part.is_empty() { 0 } else { frac_part.parse().ok()? };
    let num = (int as u128).checked_mul(denom as u128)?.checked_add(frac as u128)?;
    (num <= u64::MAX as u128).then_some((num as u64, denom))
}

#[cfg(test)]
#[path = "../tests/cli/flag_probes.rs"]
mod flag_probes;

#[cfg(test)]
mod tests {
    use super::{find, parse_size_delta, SizeArg, COMMANDS, Confirmation, Slot, TargetMode};

    fn bytes(kind: i8, v: u64) -> Option<SizeArg> {
        Some(SizeArg::Bytes { kind, bytes: v })
    }

    fn percent(kind: i8, num: u64, denom: u64) -> Option<SizeArg> {
        Some(SizeArg::Percent { kind, num, denom })
    }

    /// 探针与注册表一一对应
    #[test]
    fn flag_probes_cover_the_registry() {
        use super::flag_probes::{Kind, PROBES};
        use super::FLAGS;

        let mut registered = std::collections::BTreeSet::new();
        for f in FLAGS {
            assert!(registered.insert(f.name), "duplicate registry entry: {}", f.name);
        }
        let mut probed = std::collections::BTreeSet::new();
        for p in PROBES {
            assert!(probed.insert(p.name), "duplicate probe: {}", p.name);
            let spec = FLAGS
                .iter()
                .find(|f| f.name == p.name)
                .unwrap_or_else(|| panic!("probe for unregistered flag: {}", p.name));
            match p.kind {
                Kind::Value => {
                    assert!(spec.domain.takes_value(), "{} is a boolean in the registry", p.name);
                    assert!(p.sample.is_some(), "value flag {} needs a sample", p.name);
                }
                Kind::Bool => {
                    assert!(!spec.domain.takes_value(), "{} takes a value in the registry", p.name);
                    assert!(p.sample.is_none(), "boolean flag {} must not carry a sample", p.name);
                }
            }
            let allowed = find(p.command)
                .unwrap_or_else(|| panic!("probe names unknown command: {}", p.command));
            assert!(allowed.accepts(p.name), "`{}` does not accept {}", p.command, p.name);
        }
        assert_eq!(probed, registered, "probe set and registry must match exactly");
    }

    /// SIZE 解析（单位 k/m/g/t = 1024 进制、无单位 = 字节、小数、+/- 增量、+N%/-N%）
    #[test]
    fn size_delta_parsing() {
        const G: u64 = 1024 * 1024 * 1024;
        assert_eq!(parse_size_delta("1024"), bytes(0, 1024));
        assert_eq!(parse_size_delta("10G"), bytes(0, 10 * G));
        assert_eq!(parse_size_delta("10g"), bytes(0, 10 * G));
        assert_eq!(parse_size_delta("512"), bytes(0, 512));
        assert_eq!(parse_size_delta("+2k"), bytes(1, 2048));
        assert_eq!(parse_size_delta("-500M"), bytes(-1, 500 * 1024 * 1024));
        assert_eq!(parse_size_delta("1T"), bytes(0, 1024 * G));
        assert_eq!(parse_size_delta("1.5G"), bytes(0, 1610612736));
        assert_eq!(parse_size_delta("+1.5G"), bytes(1, 1610612736));
        assert_eq!(parse_size_delta(".5G"), bytes(0, 1024 * 1024 * 1024 / 2));
        assert_eq!(parse_size_delta("0.5M"), bytes(0, 1024 * 1024 / 2));
        assert_eq!(parse_size_delta("1.1G"), bytes(0, 1181116006));
        assert_eq!(parse_size_delta("+10%"), percent(1, 10, 1));
        assert_eq!(parse_size_delta("-10%"), percent(-1, 10, 1));
        assert_eq!(parse_size_delta("+1.5%"), percent(1, 15, 10));
        assert_eq!(parse_size_delta("18446744073709551615"), bytes(0, u64::MAX));
        assert_eq!(parse_size_delta("18446744073709551615G"), None);
        assert_eq!(parse_size_delta("18446744073709551616"), None);
        for bad in ["", "+", "-", "%", "10%", "+%", "G", "+G", "10KB", "10 G", "0x10", " 1G", "1b", "512B",
                    "1.G", ".", "1..5G", "1.5.5G", "1.5%", "+.G"] {
            assert_eq!(parse_size_delta(bad), None, "{bad:?} must be rejected");
        }
        assert_eq!(parse_size_delta("+10G"), bytes(1, 10 * G));
        assert_eq!(parse_size_delta("-10G"), bytes(-1, 10 * G));
        for bad in ["++10G", "--10G", "+++10G", "+-10G", "-+10G", "++10%", "+ 10G"] {
            assert_eq!(parse_size_delta(bad), None, "{bad:?} must be rejected");
        }
    }

    /// 把分区选择器写在目标后缀上的命令，块设备目标必须带 `:N` 或 `:last`。
    /// `resize` 的表步骤形态（`--end` / `--grow-to-end`）要求 `:N`（缺了退 2）；
    /// `resizefs` 只允许挂载点形式省略，块设备目标仍要 `:N` 才走离线形式
    fn assert_part_suffix_on_block_target(cmd: &str, target: &str, line: &str) {
        const PART_SUFFIX_REQUIRED: &[&str] = &[
            "resize", "resizefs", "move", "copy", "delete", "set", "check", "mkfs",
        ];
        if !PART_SUFFIX_REQUIRED.contains(&cmd) || !target.starts_with("/dev/") {
            return;
        }
        let suffix = target.rsplit_once(':').map(|(_, p)| p).unwrap_or("");
        assert!(
            (suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty())
                || suffix.eq_ignore_ascii_case("last"),
            "{cmd}: block-device target {target} has no partition selector :N: {line}"
        );
    }

    /// USAGE 的 examples 段与各命令的详助正文：命令名必须真实存在；把分区选择器
    /// 写在目标后缀上的命令，块设备目标必须带 `:N`
    #[test]
    fn usage_examples_follow_the_command_syntax() {
        let examples = crate::docs::EXAMPLES;
        assert!(!examples.is_empty(), "USAGE must keep an examples block");
        for line in examples {
            let mut it = line.split_whitespace();
            assert_eq!(it.next(), Some("diskedit"), "example must start with the binary name: {line}");
            let cmd = it.next().unwrap_or_else(|| panic!("example has no command: {line}"));
            assert!(find(cmd).is_some(), "example names unknown command {cmd}: {line}");
            let Some(target) = it.next() else { continue };
            assert_part_suffix_on_block_target(cmd, target, line);
        }
        for def in COMMANDS {
            for line in def.details.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("diskedit ") else { continue };
                let mut it = rest.split_whitespace();
                let Some(cmd) = it.next() else { continue };
                let Some(target) = it.next() else { continue };
                assert_part_suffix_on_block_target(cmd, target, line);
            }
        }
    }

    /// 选项总表与注册表双向对账。两个语种各有一份表，逐份比对
    #[test]
    fn internals_options_match_the_registry() {
        const DOCS: &[(&str, &str, &str)] = &[
            ("docs/en/internals.md", "## Options", include_str!("../docs/en/internals.md")),
            ("docs/zh/internals.md", "## 选项", include_str!("../docs/zh/internals.md")),
        ];
        for (doc, heading, text) in DOCS {
            check_options_table(doc, heading, text);
        }

        fn check_options_table(doc: &str, heading: &str, text: &str) {
            use super::{flag_spec, FLAGS};

            let rows: Vec<&str> = text
                .lines()
                .skip_while(|l| l.trim() != heading)
                .skip(1)
                .take_while(|l| !l.starts_with("## "))
                .collect();
            assert!(!rows.is_empty(), "{doc} must carry a `{heading}` section");

            const PIPE: char = '\u{00A6}';
            let mut documented: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for line in rows {
                let row = line.replace("\\|", &PIPE.to_string());
                let Some(rest) = row.strip_prefix('|') else { continue };
                let cells: Vec<&str> = rest.split('|').collect();
                let cell = cells[0].trim();
                if !cell.starts_with("`--") {
                    continue;
                }
                let name = cell.trim_matches('`');
                assert!(
                    documented.insert(name.to_string()),
                    "{doc} lists {name} twice"
                );
                let spec = flag_spec(name)
                    .unwrap_or_else(|| panic!("{doc} lists an unregistered flag: {name}"));
                let value = cells[1].trim().replace(PIPE, "|");
                if spec.domain.takes_value() {
                    let placeholder = spec.domain.placeholder();
                    assert_ne!(value, "—", "{name} takes a value but the table calls it a boolean");
                    assert_eq!(
                        value, placeholder,
                        "{doc}: {name} placeholder must match the registry"
                    );
                } else {
                    assert_eq!(
                        value, "—",
                        "{name} is a boolean in the registry, so the table must give it no value"
                    );
                }
            }
            let registered: std::collections::BTreeSet<String> =
                FLAGS.iter().map(|f| f.name.to_string()).collect();
            assert_eq!(
                documented, registered,
                "the options table in {doc} and the registry must match exactly"
            );
        }
    }

    /// 文档里的示例要能直接跑通：`Required` 的命令，示例行必须带 `--yes`。
    /// 用法行（含 `<...>` 占位符）不在此列——那里用 `[OPTIONS]` 概括，不逐旗标展开。
    #[test]
    fn documented_examples_pass_the_confirmation_gate() {
        const DOCS: &[(&str, &str)] = &[
            ("README.md", include_str!("../README.md")),
            ("README.zh-CN.md", include_str!("../README.zh-CN.md")),
            ("docs/en/interop.md", include_str!("../docs/en/interop.md")),
            ("docs/en/reference.md", include_str!("../docs/en/reference.md")),
            ("docs/zh/interop.md", include_str!("../docs/zh/interop.md")),
            ("docs/zh/internals.md", include_str!("../docs/zh/internals.md")),
            ("docs/zh/reference.md", include_str!("../docs/zh/reference.md")),
        ];
        let mut checked = 0;
        for (file, text) in DOCS {
            for (index, line) in text.lines().enumerate() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("diskedit ") else { continue };
                if line.contains('<') {
                    continue;
                }
                let mut words = rest.split_whitespace();
                let head = words.next().unwrap_or("");
                let tail = words.next().unwrap_or("");
                let group = format!("{head} {tail}");
                let Some(def) = COMMANDS
                    .iter()
                    .find(|c| c.name == head)
                    .or_else(|| COMMANDS.iter().find(|c| c.name == group.as_str()))
                else {
                    continue;
                };
                checked += 1;
                if def.confirmation == Confirmation::Required {
                    assert!(
                        line.contains("--yes"),
                        "{file}:{}: an example for `{}` must pass --yes, or it exits 2: {line}",
                        index + 1,
                        def.name
                    );
                }
            }
        }
        assert!(checked >= 8, "the scan found only {checked} example lines");
    }

    /// 命令表的自洽：名字互不重复；每条命令都有摘要、用法与详助；第一条位置参数是
    /// 命令的作用对象（目标、记录编号或容器根）；详助由生成器以命令名开头
    #[test]
    fn command_table_is_self_consistent() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.name), "duplicate command name {}", c.name);
            assert!(!c.summary.is_empty(), "{}: missing summary", c.name);
            assert!(!c.details.is_empty(), "{}: missing details", c.name);
            assert!(!c.variants.is_empty(), "{}: missing forms", c.name);
            assert!(
                c.variants.iter().filter(|v| v.default).count() <= 1,
                "{}: more than one default form",
                c.name
            );
            for v in c.variants {
                assert!(!v.syntax.is_empty(), "{}: form without syntax", c.name);
                // 槽位在单个形态内唯一：同一命令的多个形态共用作用对象槽位是正常的
                // （`info <TARGET>` 与 `info <TARGET>:N --grow`）
                let mut slots = std::collections::HashSet::new();
                assert!(
                    matches!(
                        v.positionals.first().map(|p| p.slot),
                        Some(Slot::Target | Slot::OptionalTarget | Slot::RecordId | Slot::RootDir)
                    ),
                    "{}: first positional must name the thing the command acts on",
                    c.name
                );
                assert!(
                    v.positionals.iter().all(|p| !p.optional)
                        || v.positionals.last().is_some_and(|p| p.optional),
                    "{}: only the trailing positional may be optional",
                    c.name
                );
                for p in v.positionals {
                    assert!(slots.insert(p.slot), "{}: duplicate slot {:?}", c.name, p.slot);
                    assert!(!p.name.is_empty(), "{}: positional needs a name", c.name);
                    assert!(p.domain.takes_value(), "{}: {} has no value domain", c.name, p.name);
                }
                for f in v.options {
                    assert!(f.name.starts_with("--"), "{}: {} is not a long option", c.name, f.name);
                }
            }
            let help = crate::docs::help_text(c.name)
                .unwrap_or_else(|| panic!("{}: help is not generated", c.name));
            assert!(
                help.starts_with(&format!("diskedit {}", c.name)),
                "{}: help text must begin with the command name",
                c.name
            );
        }
    }

    /// 白名单与旗标注册表的对账
    #[test]
    fn flag_whitelists_match_the_registry() {
        use super::FLAGS;
        let registry: Vec<&str> = FLAGS.iter().map(|f| f.name).collect();
        for c in COMMANDS {
            for v in c.variants {
                for f in v.options {
                    assert!(registry.contains(&f.name), "{}: {} is not in the flag registry", c.name, f.name);
                }
            }
            for f in c.common_options {
                assert!(registry.contains(&f.name), "{}: {} is not in the flag registry", c.name, f.name);
            }
        }
        for spec in FLAGS {
            assert!(
                COMMANDS.iter().any(|c| c.accepts(spec.name)),
                "{}: registered but no command accepts it",
                spec.name
            );
        }
    }

    /// 白名单是 fail-closed 的：起始/结束旗标只归登记在册的命令
    #[test]
    fn flag_whitelists() {
        assert!(!find("resize").unwrap().accepts("--start"));
        assert!(find("resize").unwrap().accepts("--end"));
        assert!(find("resize").unwrap().accepts("--align"));
        assert!(find("resize").unwrap().accepts("--no-fs"));
        assert!(!find("resize").unwrap().accepts("--chunk-size"));
        assert!(find("move").unwrap().accepts("--start"));
        assert!(find("nosuchcmd").is_none());
    }

    /// 对齐只有 mib / none：帮助与用法里不得出现柱面对齐或 CHS 几何
    #[test]
    fn help_never_advertises_cyl_alignment() {
        let mut texts = vec![crate::docs::usage_text()];
        texts.extend(COMMANDS.iter().filter_map(|c| crate::docs::help_text(c.name)));
        for text in texts {
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("cyl"), "help text advertises cylinder alignment");
            assert!(!lower.contains("chs"), "help text advertises CHS geometry");
        }
    }

    /// 确认规则是逐条裁定的
    #[test]
    fn confirmation_rules_are_as_reviewed() {
        let expected: &[(&str, Confirmation)] = &[
            ("info", Confirmation::None),
            ("inspect", Confirmation::None),
            ("records list", Confirmation::None),
            ("records verify", Confirmation::None),
            ("records scan", Confirmation::None),
            ("resize", Confirmation::Required),
            ("move", Confirmation::Required),
            ("copy", Confirmation::Required),
            ("delete", Confirmation::Required),
            ("set", Confirmation::Required),
            ("check", Confirmation::None),
            ("mkfs", Confirmation::Required),
            ("resizefs", Confirmation::Required),
            ("recover", Confirmation::Required),
            ("new", Confirmation::Required),
            ("add", Confirmation::Required),
        ];
        for (name, rule) in expected {
            let c = find(name).unwrap_or_else(|| panic!("{name}: not a command"));
            assert_eq!(c.confirmation, *rule, "{name}: confirmation rule changed");
        }
        for c in COMMANDS {
            assert!(
                expected.iter().any(|(n, _)| *n == c.name),
                "{}: no expected confirmation rule — decide it and add it here",
                c.name
            );
        }
    }

    /// 写入模式的命令都要打开目标：镜像的扇区大小没有别的来源，给了覆盖才读得对。
    /// 集合级约束——新增写入命令必须自己声明这个开关
    #[test]
    fn write_commands_take_sector_size() {
        for c in COMMANDS.iter().filter(|c| c.mode != TargetMode::ReadOnly) {
            assert!(
                c.accepts("--sector-size"),
                "{}: writes to the target but does not take --sector-size",
                c.name
            );
        }
    }

    /// 带参旗标的两种取值写法落到同一字段；相对尺寸只在 SIZE 槽位放行
    #[test]
    fn value_flags_and_size_slot() {
        use std::ffi::OsString;

        fn parse(tokens: &[&str]) -> super::Request {
            let argv: Vec<OsString> = tokens.iter().copied().map(OsString::from).collect();
            super::parse_from(argv.into_iter()).1
        }

        let r = parse(&[
            "copy", "img:1", "--start", "1000", "--chunk", "4", "--name", "valid-name", "--yes",
        ]);
        match r {
            super::Request::Copy(c) => {
                assert_eq!(c.name.as_deref(), Some("valid-name"));
                assert_eq!(c.start, Some(super::StartSelector::Lba(1000)));
            }
            _ => panic!("expected a copy request"),
        }

        let r = parse(&[
            "copy", "img:1", "--start", "1000", "--chunk", "4", "--name=-leading-value", "--yes",
        ]);
        match r {
            super::Request::Copy(c) => assert_eq!(c.name.as_deref(), Some("-leading-value")),
            _ => panic!("expected a copy request"),
        }

        let r = parse(&["resize", "img:1", "-10", "--yes"]);
        match r {
            super::Request::Resize(c) => {
                assert_eq!(c.size, super::SizeSpec::Delta { grow: false, bytes: 10 });
            }
            _ => panic!("expected a resize request"),
        }

        let r = parse(&["info", "img", "--sector-size", "65536"]);
        match r {
            super::Request::Info(c) => assert_eq!(c.sector_size.map(|s| s.get()), Some(65536)),
            _ => panic!("expected an info request"),
        }
    }

    /// 显式旗标与默认值必须可区分
    #[test]
    fn explicit_flags_are_recorded() {
        use std::ffi::OsString;

        fn parse(tokens: &[&str]) -> super::Request {
            let argv: Vec<OsString> = tokens.iter().copied().map(OsString::from).collect();
            super::parse_from(argv.into_iter()).1
        }

        let super::Request::Move(r) =
            parse(&["move", "img:1", "--start", "2048", "--chunk", "4", "--yes"])
        else {
            panic!("expected a move request");
        };
        assert!(!r.align_given, "--align was not given");
        assert_eq!(r.align, crate::domain::Align::none());

        let super::Request::Resize(r) = parse(&["resize", "img:1", "grow", "--yes"]) else {
            panic!("expected a resize request");
        };
        assert_eq!(r.chunk_mib, None, "an omitted --chunk stays distinguishable from an explicit one");

        let super::Request::Move(r) = parse(&[
            "move", "img:1", "--start", "2048", "--align", "mib", "--chunk", "8", "--yes",
        ]) else {
            panic!("expected a move request");
        };
        assert!(r.align_given, "explicit --align mib must be distinguishable from the default");
        assert_eq!(r.align, crate::domain::Align::mib(1).unwrap());
        assert_eq!(r.chunk_mib, Some(8));

        let super::Request::Move(r) = parse(&["move", "img:1", "--start", "align", "--yes"]) else {
            panic!("expected a move request");
        };
        assert_eq!(r.start, Some(super::StartSelector::Aligned), "`align` is a selector, not an LBA");

        let super::Request::Resize(r) =
            parse(&["resize", "img:1", "grow", "--chunk", "8", "--yes"])
        else {
            panic!("expected a resize request");
        };
        assert_eq!(r.chunk_mib, Some(8));
    }
}
