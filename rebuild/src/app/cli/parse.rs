//! `argv → Request`：按命令声明收选项与位置参数，只做字面量解析。
//!
//! 不认识目标、分区表与文件系统；所有事实判断都留给观察与判定层。

use std::collections::BTreeMap;
use std::path::PathBuf;

use diskedit_core::fs::MkfsKind;
use diskedit_core::table::Guid;

use super::spec::{self, Spec, TargetMode};
use crate::app::observe::{
    AddRequest, Command, CopyRequest, MoveRequest, PartitionSelector, RecoverMode, Request,
    ResizeRequest, SetAction, SizeSpec, StartSpec, TableFormat, TargetSpec,
};

/// 已收齐的选项：名字到可选取值。
type Flags = BTreeMap<String, Option<String>>;

/// 一次已解析的调用。
pub struct Invocation {
    pub request: Request,
    pub yes: bool,
    pub dry_run: bool,
}

/// 解析进程参数（含程序名）。
pub fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Invocation, String> {
    let mut tokens = Vec::new();
    for arg in args {
        let text = arg
            .into_string()
            .map_err(|_| "argument is not valid UTF-8".to_owned())?;
        tokens.push(text);
    }
    if !tokens.is_empty() {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        return Err("missing command".to_owned());
    }
    if tokens[0] == "--help" || tokens[0] == "-h" {
        spec::print_help();
        return Err(String::new());
    }
    if tokens[0] == "--version" {
        println!("diskedit {}", env!("CARGO_PKG_VERSION"));
        return Err(String::new());
    }

    let (name, consumed) = if tokens[0] == "records" {
        let sub = tokens
            .get(1)
            .ok_or_else(|| "records requires a subcommand".to_owned())?;
        (format!("records {sub}"), 2)
    } else {
        (tokens[0].clone(), 1)
    };
    let spec = spec::find(&name).ok_or_else(|| format!("unknown command {name}"))?;
    let (positionals, flags) = collect(&tokens[consumed..], spec)?;
    build(name.as_str(), spec, &positionals, &flags)
}

/// 收齐位置参数与选项；未知选项、重复选项、缺值都落用法错误。
fn collect(rest: &[String], spec: &Spec) -> Result<(Vec<String>, Flags), String> {
    let mut positionals = Vec::new();
    let mut flags: Flags = BTreeMap::new();
    let mut index = 0;
    while index < rest.len() {
        let token = &rest[index];
        if token == "--" {
            positionals.extend(rest[index + 1..].iter().cloned());
            break;
        }
        if token.starts_with("--") {
            let (name, inline) = match token.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (token.clone(), None),
            };
            let value_needed = spec
                .options
                .iter()
                .find(|opt| opt.name == name)
                .map(|opt| opt.value);
            let Some(value_needed) = value_needed else {
                return Err(format!("unknown option {name}"));
            };
            if flags.contains_key(&name) {
                return Err(format!("option {name} given more than once"));
            }
            if value_needed {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        rest.get(index)
                            .cloned()
                            .ok_or_else(|| format!("option {name} requires a value"))?
                    }
                };
                flags.insert(name, Some(value));
            } else {
                if inline.is_some() {
                    return Err(format!("option {name} does not take a value"));
                }
                flags.insert(name, None);
            }
        } else if token.starts_with('-') && token != "-" {
            return Err(format!("unknown option {token}"));
        } else {
            positionals.push(token.clone());
        }
        index += 1;
    }
    Ok((positionals, flags))
}

fn flag(flags: &Flags, name: &str) -> bool {
    flags.contains_key(name)
}

fn value(flags: &Flags, name: &str) -> Result<Option<String>, String> {
    Ok(flags.get(name).and_then(|value| value.clone()))
}

fn required_value(flags: &Flags, name: &str) -> Result<String, String> {
    value(flags, name)?.ok_or_else(|| format!("option {name} requires a value"))
}

fn build(
    name: &str,
    spec: &Spec,
    positionals: &[String],
    flags: &Flags,
) -> Result<Invocation, String> {
    let yes = flag(flags, "--yes");
    let sector_size = match value(flags, "--sector-size")? {
        Some(text) => Some(parse_sector_size(&text)?),
        None => None,
    };
    let mut invocation = match name {
        "info" => {
            let target = target_at(
                positionals,
                0,
                if flag(flags, "--grow") {
                    TargetMode::Partition
                } else {
                    TargetMode::Whole
                },
            )?;
            expect_count(positionals, 1, 1)?;
            let request = Request::new(target, Command::Info);
            finish(request, false, sector_size)
        }
        "inspect" => {
            expect_count(positionals, 1, 1)?;
            let target = target_at(positionals, 0, spec.target)?;
            finish(Request::new(target, Command::Inspect), false, sector_size)
        }
        "records list" => {
            expect_count(positionals, 0, 1)?;
            let request = match positionals.first() {
                Some(_) => {
                    let target = target_at(positionals, 0, TargetMode::Whole)?;
                    Request::new(target, Command::RecordsList)
                }
                None => Request::without_target(Command::RecordsList),
            };
            finish(request, false, sector_size)
        }
        "records verify" => {
            expect_count(positionals, 2, 2)?;
            let target = target_at(positionals, 0, spec.target)?;
            let token = positionals[1].clone();
            if token.len() != 32 || !token.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err("record id must be 32 hex digits".to_owned());
            }
            finish(
                Request::new(target, Command::RecordsVerify { token }),
                false,
                sector_size,
            )
        }
        "records scan" => {
            expect_count(positionals, 1, 1)?;
            let target = target_at(positionals, 0, spec.target)?;
            finish(
                Request::new(target, Command::RecordsScan),
                false,
                sector_size,
            )
        }
        "resize" => {
            expect_count(positionals, 1, 2)?;
            let target = target_path(positionals, 0)?;
            let partition = target_at(positionals, 0, spec.target)?.partition;
            let size = positionals
                .get(1)
                .map(|text| parse_size_spec(text))
                .transpose()?;
            let end = value(flags, "--end")?
                .map(|text| parse_lba(&text))
                .transpose()?;
            let grow_to_end = flag(flags, "--grow-to-end");
            if end.is_some() && grow_to_end {
                return Err("--end conflicts with --grow-to-end".to_owned());
            }
            if size.is_some() && (end.is_some() || grow_to_end) {
                return Err("size conflicts with --end/--grow-to-end".to_owned());
            }
            if size.is_none() && end.is_none() && !grow_to_end {
                return Err("resize requires SIZE, --end, or --grow-to-end".to_owned());
            }
            if (end.is_some() || grow_to_end) && partition.is_none() {
                return Err("this form requires a partition selector :N or :last".to_owned());
            }
            finish(
                Request::new(
                    target,
                    Command::Resize(ResizeRequest {
                        partition,
                        size,
                        end,
                        grow_to_end,
                        align: value(flags, "--align")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                        allow_move: flag(flags, "--allow-move"),
                        chunk: value(flags, "--chunk")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                        grow_lv: flag(flags, "--grow-lv"),
                        lv: value(flags, "--lv")?,
                        no_fs: flag(flags, "--no-fs"),
                        dry_run: flag(flags, "--dry-run"),
                    }),
                ),
                flag(flags, "--dry-run"),
                sector_size,
            )
        }
        "move" => {
            expect_count(positionals, 1, 1)?;
            let target = target_path(positionals, 0)?;
            let partition = require_partition(&target)?;
            finish(
                Request::new(
                    target,
                    Command::Move(MoveRequest {
                        partition,
                        start: parse_start(&required_value(flags, "--start")?)?,
                        align: value(flags, "--align")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                        chunk: value(flags, "--chunk")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                    }),
                ),
                false,
                sector_size,
            )
        }
        "copy" => {
            expect_count(positionals, 1, 1)?;
            let target = target_path(positionals, 0)?;
            let partition = require_partition(&target)?;
            let start = parse_start(&required_value(flags, "--start")?)?;
            if matches!(start, StartSpec::Align) {
                return Err("copy --start accepts only an LBA or end".to_owned());
            }
            finish(
                Request::new(
                    target,
                    Command::Copy(CopyRequest {
                        partition,
                        start,
                        align: value(flags, "--align")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                        chunk: value(flags, "--chunk")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                        name: value(flags, "--name")?,
                        keep_partuuid: flag(flags, "--keep-partuuid"),
                    }),
                ),
                false,
                sector_size,
            )
        }
        "delete" => {
            expect_count(positionals, 1, 1)?;
            let target = target_path(positionals, 0)?;
            let partition = require_partition(&target)?;
            finish(
                Request::new(target, Command::Delete { partition }),
                false,
                sector_size,
            )
        }
        "set" => {
            let (target, action) = parse_set(positionals, flags)?;
            let partition = partition_of(&target)?;
            finish(
                Request::new(target, Command::Set { partition, action }),
                false,
                sector_size,
            )
        }
        "check" => {
            expect_count(positionals, 1, 1)?;
            let target = target_path(positionals, 0)?;
            let partition = require_partition(&target)?;
            finish(
                Request::new(target, Command::Check { partition }),
                false,
                sector_size,
            )
        }
        "mkfs" => {
            expect_count(positionals, 2, 2)?;
            let target = target_path(positionals, 0)?;
            let partition = require_partition(&target)?;
            let filesystem = MkfsKind::parse(&positionals[1])
                .ok_or_else(|| format!("unknown filesystem {}", positionals[1]))?;
            finish(
                Request::new(
                    target,
                    Command::Mkfs {
                        partition,
                        filesystem,
                        label: value(flags, "--label")?,
                    },
                ),
                false,
                sector_size,
            )
        }
        "resizefs" => {
            expect_count(positionals, 1, 2)?;
            let target = target_path(positionals, 0)?;
            let partition = target.partition;
            let bytes = positionals
                .get(1)
                .map(|text| parse_size(text))
                .transpose()?;
            // 离线形态把文件系统撑满分区，没有尺寸参数；尺寸只在挂载点在线形态出现，
            // 这里拒绝，避免"报成功却没扩到请求尺寸"。
            if partition.is_some() && bytes.is_some() {
                return Err("offline resizefs does not take a size: it grows the filesystem to fill its partition".to_owned());
            }
            // 挂载点形态反查的是宿主现场，逻辑扇区大小由内核给出，不由用户覆盖。
            if partition.is_none() && sector_size.is_some() {
                return Err("mount-point form does not take --sector-size: the sector size comes from the kernel".to_owned());
            }
            finish(
                Request::new(
                    target,
                    Command::Resizefs {
                        partition,
                        bytes,
                        mount: false,
                    },
                ),
                false,
                sector_size,
            )
        }
        "recover" => {
            expect_count(positionals, 1, 1)?;
            let target = target_at(positionals, 0, spec.target)?;
            let replay = flag(flags, "--replay-table-only");
            let release = flag(flags, "--release");
            let discard = flag(flags, "--discard-replay");
            if discard && !release {
                return Err("--discard-replay requires --release".to_owned());
            }
            if replay && release {
                return Err("--replay-table-only conflicts with --release".to_owned());
            }
            let mode = if release {
                RecoverMode::Release {
                    discard_replay: discard,
                }
            } else if replay {
                RecoverMode::ReplayTableOnly
            } else {
                RecoverMode::Default
            };
            finish(
                Request::new(target, Command::Recover(mode)),
                false,
                sector_size,
            )
        }
        "new" => {
            expect_count(positionals, 1, 1)?;
            let target = target_at(positionals, 0, spec.target)?;
            let table = match value(flags, "--table")?.as_deref() {
                None | Some("gpt") => TableFormat::Gpt,
                Some("mbr") => TableFormat::Mbr,
                Some(other) => return Err(format!("--table accepts only gpt or mbr, got {other}")),
            };
            finish(
                Request::new(
                    target,
                    Command::New {
                        table,
                        overwrite: flag(flags, "--overwrite-data"),
                    },
                ),
                false,
                sector_size,
            )
        }
        "add" => {
            expect_count(positionals, 2, 2)?;
            let target = target_at(positionals, 0, spec.target)?;
            let size = parse_size(&positionals[1])?;
            let part_number = value(flags, "--part-number")?
                .map(|text| parse_part_number(&text))
                .transpose()?;
            finish(
                Request::new(
                    target,
                    Command::Add(AddRequest {
                        size,
                        start: value(flags, "--start")?
                            .map(|text| parse_lba(&text))
                            .transpose()?,
                        align: value(flags, "--align")?
                            .map(|text| parse_size(&text))
                            .transpose()?,
                        part_number,
                        name: value(flags, "--name")?,
                        type_guid: value(flags, "--type-guid")?
                            .map(|text| parse_guid(&text))
                            .transpose()?,
                    }),
                ),
                false,
                sector_size,
            )
        }
        other => return Err(format!("unknown command {other}")),
    };
    invocation.yes = yes;
    if let Some(raw) = value(flags, "--identity")? {
        invocation.request = invocation
            .request
            .with_identity(Some(validate_identity(&raw)?));
    }
    if spec.confirmation && !yes {
        return Err(format!("write command {} requires --yes", spec.name));
    }
    Ok(invocation)
}

/// `--identity` 的合法性：去空白后非空、不含控制字符、长度不超过 255 字节。
///
/// 名字会进入记录的命名空间与锁的键，控制字符或超长名字会污染落点。
fn validate_identity(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("--identity requires a non-empty name".to_owned());
    }
    if name.chars().any(char::is_control) {
        return Err("--identity cannot contain control characters".to_owned());
    }
    if name.len() > 255 {
        return Err("--identity exceeds 255 bytes".to_owned());
    }
    Ok(name.to_owned())
}

fn finish(mut request: Request, dry_run: bool, sector_size: Option<u64>) -> Invocation {
    if let Some(sector) = sector_size {
        request = request.with_sector_size(Some(sector));
    }
    Invocation {
        yes: false,
        request,
        dry_run,
    }
}

fn expect_count(positionals: &[String], min: usize, max: usize) -> Result<(), String> {
    if positionals.len() < min || positionals.len() > max {
        return Err(format!(
            "wrong number of positional arguments: expected {min}..={max}, got {}",
            positionals.len()
        ));
    }
    Ok(())
}

fn target_path(positionals: &[String], index: usize) -> Result<TargetSpec, String> {
    let raw = positionals
        .get(index)
        .ok_or_else(|| "missing target argument".to_owned())?;
    parse_target(raw)
}

fn target_at(positionals: &[String], index: usize, mode: TargetMode) -> Result<TargetSpec, String> {
    let target = target_path(positionals, index)?;
    match mode {
        TargetMode::Whole | TargetMode::None => {
            if target.partition.is_some() {
                return Err("this command's target does not accept a :N or :last suffix".to_owned());
            }
        }
        TargetMode::Partition => {
            if target.partition.is_none() {
                return Err("this command's target requires a :N or :last suffix".to_owned());
            }
        }
        TargetMode::Any => {}
    }
    Ok(target)
}

fn require_partition(target: &TargetSpec) -> Result<PartitionSelector, String> {
    target
        .partition
        .ok_or_else(|| "this command's target requires a :N or :last suffix".to_owned())
}

fn partition_of(target: &TargetSpec) -> Result<PartitionSelector, String> {
    require_partition(target)
}

fn parse_target(raw: &str) -> Result<TargetSpec, String> {
    if let Some((path, suffix)) = raw.rsplit_once(':') {
        if suffix == "last" {
            return Ok(TargetSpec::new(path, Some(PartitionSelector::Last)));
        }
        if let Ok(number) = suffix.parse::<u32>() {
            if number == 0 {
                return Err("partition number cannot be 0".to_owned());
            }
            if number > 128 {
                return Err(format!("partition number {number} out of range 1..=128"));
            }
            return Ok(TargetSpec::new(
                path,
                Some(PartitionSelector::Number(number)),
            ));
        }
    }
    Ok(TargetSpec::new(PathBuf::from(raw), None))
}

fn parse_set(positionals: &[String], flags: &Flags) -> Result<(TargetSpec, SetAction), String> {
    if positionals.len() < 2 {
        return Err("set requires a target and a property name".to_owned());
    }
    let target = parse_target(&positionals[0])?;
    let property = positionals[1].as_str();
    let rest = &positionals[2..];
    let action = match property {
        "name" => {
            expect_exact(rest, 1)?;
            SetAction::Name(rest[0].clone())
        }
        "label" => {
            expect_exact(rest, 1)?;
            SetAction::Label(rest[0].clone())
        }
        "uuid" => {
            if rest.len() > 1 {
                return Err("set uuid takes at most one value".to_owned());
            }
            let random = flag(flags, "--random");
            if rest.is_empty() && !random {
                return Err("set uuid requires a value or --random".to_owned());
            }
            SetAction::Uuid {
                value: rest.first().cloned(),
                random,
            }
        }
        "flag" => {
            expect_exact(rest, 2)?;
            let on = match rest[1].as_str() {
                "on" => true,
                "off" => false,
                other => return Err(format!("flag state {other} must be on or off")),
            };
            SetAction::Flag {
                flag: rest[0].clone(),
                on,
            }
        }
        "type" => {
            expect_exact(rest, 1)?;
            SetAction::Type(rest[0].clone())
        }
        other => return Err(format!("unknown property {other}")),
    };
    if flag(flags, "--random") && property != "uuid" {
        return Err("--random is only for set uuid".to_owned());
    }
    Ok((target, action))
}

fn expect_exact(rest: &[String], count: usize) -> Result<(), String> {
    if rest.len() != count {
        return Err(format!(
            "wrong number of values for this property: expected {count}, got {}",
            rest.len()
        ));
    }
    Ok(())
}

fn parse_start(text: &str) -> Result<StartSpec, String> {
    match text {
        "end" => Ok(StartSpec::End),
        "align" => Ok(StartSpec::Align),
        other => Ok(StartSpec::Lba(parse_lba(other)?)),
    }
}

fn parse_lba(text: &str) -> Result<u64, String> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| format!("{text} is not a valid LBA"))
    } else {
        text.parse::<u64>()
            .map_err(|_| format!("{text} is not a valid LBA"))
    }
}

fn parse_part_number(text: &str) -> Result<u32, String> {
    let number = text
        .parse::<u32>()
        .map_err(|_| format!("partition number {text} is not an integer"))?;
    if !(1..=128).contains(&number) {
        return Err(format!("partition number {number} out of range 1..=128"));
    }
    Ok(number)
}

fn parse_sector_size(text: &str) -> Result<u64, String> {
    let sector = text
        .parse::<u64>()
        .map_err(|_| format!("sector size {text} is not an integer"))?;
    const ALLOWED: [u64; 8] = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];
    if !ALLOWED.contains(&sector) {
        return Err(format!("sector size {sector} is not allowed"));
    }
    Ok(sector)
}

fn parse_size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty size".to_owned());
    }
    if trimmed.starts_with('+') || trimmed.starts_with('-') || trimmed.ends_with('%') {
        return Err("only absolute sizes are supported".to_owned());
    }
    let lowered = trimmed.to_ascii_lowercase();
    let (digits, multiplier) = [
        ("kib", 1u64 << 10),
        ("ki", 1u64 << 10),
        ("kb", 1u64 << 10),
        ("k", 1u64 << 10),
        ("mib", 1u64 << 20),
        ("mi", 1u64 << 20),
        ("mb", 1u64 << 20),
        ("m", 1u64 << 20),
        ("gib", 1u64 << 30),
        ("gi", 1u64 << 30),
        ("gb", 1u64 << 30),
        ("g", 1u64 << 30),
        ("tib", 1u64 << 40),
        ("ti", 1u64 << 40),
        ("tb", 1u64 << 40),
        ("t", 1u64 << 40),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        lowered
            .strip_suffix(suffix)
            .map(|digits| (digits.to_owned(), multiplier))
    })
    .unwrap_or((lowered.clone(), 1));
    let base = digits
        .parse::<u64>()
        .map_err(|_| format!("size {text} is not a valid number"))?;
    base.checked_mul(multiplier)
        .ok_or_else(|| format!("size {text} out of range"))
}

/// 解析 `resize` 的 SIZE 写法：`grow`、绝对字节、`+N`/`-N`、`+N%`/`-N%`。
fn parse_size_spec(text: &str) -> Result<SizeSpec, String> {
    let trimmed = text.trim();
    if trimmed == "grow" {
        return Ok(SizeSpec::Grow);
    }
    if let Some(body) = trimmed.strip_prefix('+') {
        return parse_relative_size(true, body);
    }
    if let Some(body) = trimmed.strip_prefix('-') {
        return parse_relative_size(false, body);
    }
    if trimmed.ends_with('%') {
        return Err("percent must use +N% or -N% form".to_owned());
    }
    Ok(SizeSpec::Absolute(parse_size(trimmed)?))
}

fn parse_relative_size(grow: bool, body: &str) -> Result<SizeSpec, String> {
    if let Some(percent) = body.strip_suffix('%') {
        let (num, denom) = parse_percent(percent)?;
        return Ok(SizeSpec::Percent { grow, num, denom });
    }
    Ok(SizeSpec::Delta {
        grow,
        bytes: parse_size(body)?,
    })
}

/// 十进制百分比 →（分子, 分母）；`10` 为 `10/1`，`1.5` 为 `15/10`。
fn parse_percent(text: &str) -> Result<(u64, u64), String> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 9 {
        return Err(format!("too many decimal places in percent {text}%"));
    }
    let denom = 10u64
        .checked_pow(u32::try_from(fraction.len()).map_err(|_| "percent too long".to_owned())?)
        .ok_or_else(|| format!("percent {text}% out of range"))?;
    let whole = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .map_err(|_| format!("percent {text}% is not a valid number"))?
    };
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u64>()
            .map_err(|_| format!("percent {text}% is not a valid number"))?
    };
    let num = whole
        .checked_mul(denom)
        .and_then(|scaled| scaled.checked_add(fraction))
        .ok_or_else(|| format!("percent {text}% out of range"))?;
    Ok((num, denom))
}

fn parse_guid(text: &str) -> Result<Guid, String> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        if hex.len() > 2 {
            return Err(format!("{text} is not in 0xXX form"));
        }
        let byte =
            u8::from_str_radix(hex, 16).map_err(|_| format!("{text} is not a valid byte"))?;
        let mut bytes = [0u8; 16];
        bytes[0] = byte;
        return Ok(Guid(bytes));
    }
    let hex: String = text.chars().filter(|ch| *ch != '-').collect();
    if hex.len() != 32 {
        return Err(format!("GUID {text} has an invalid length"));
    }
    let mut bytes = [0u8; 16];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let pair = &hex[index * 2..index * 2 + 2];
        *slot = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("GUID {text} contains non-hex characters"))?;
    }
    Ok(Guid(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn run(args: &[&str]) -> Result<Invocation, String> {
        let mut tokens = vec![OsString::from("diskedit")];
        tokens.extend(args.iter().map(OsString::from));
        parse(tokens)
    }

    fn token() -> String {
        "0123456789abcdef0123456789abcdef".to_owned()
    }

    #[test]
    fn every_entry_has_a_minimal_working_form() {
        let token = token();
        let cases: Vec<Vec<&str>> = vec![
            vec!["info", "/img"],
            vec!["inspect", "/img"],
            vec!["records", "list"],
            vec!["records", "verify", "/img", token.as_str()],
            vec!["records", "scan", "/root"],
            vec!["resize", "/img:1", "10M", "--yes"],
            vec!["move", "/img:1", "--start", "end", "--yes"],
            vec!["copy", "/img:1", "--start", "end", "--yes"],
            vec!["delete", "/img:1", "--yes"],
            vec!["set", "/img:1", "name", "root", "--yes"],
            vec!["set", "/img:1", "flag", "2", "on", "--yes"],
            vec!["set", "/img:1", "uuid", "--random", "--yes"],
            vec!["check", "/img:1"],
            vec!["mkfs", "/img:1", "ext4", "--yes"],
            vec!["resizefs", "/img:1", "--yes"],
            vec!["recover", "/img", "--yes"],
            vec!["new", "/img", "--yes"],
            vec!["add", "/img", "16M", "--yes"],
        ];
        for case in cases {
            run(&case).unwrap_or_else(|error| panic!("{case:?} 应可解析：{error}"));
        }
    }

    #[test]
    fn unknown_option_is_a_usage_error() {
        assert!(run(&["info", "/img", "--nope"]).is_err());
    }

    #[test]
    fn write_command_without_confirmation_is_a_usage_error() {
        assert!(run(&["delete", "/img:1"]).is_err());
    }

    #[test]
    fn read_only_command_rejects_confirmation() {
        assert!(run(&["inspect", "/img", "--yes"]).is_err());
    }

    #[test]
    fn target_suffix_is_enforced() {
        assert!(run(&["delete", "/img", "--yes"]).is_err());
        assert!(run(&["new", "/img:1", "--yes"]).is_err());
        assert!(run(&["inspect", "/img:1"]).is_err());
        assert!(run(&["delete", "/img:0", "--yes"]).is_err());
    }

    #[test]
    fn sector_size_domain_is_enforced() {
        assert!(run(&["inspect", "/img", "--sector-size", "4096"]).is_err());
        assert!(run(&["info", "/img", "--sector-size", "123"]).is_err());
        assert!(run(&["info", "/img", "--sector-size", "4096"]).is_ok());
    }

    #[test]
    fn mutually_exclusive_selectors_are_rejected() {
        assert!(run(&["resize", "/img:1", "--end", "100", "--grow-to-end", "--yes"]).is_err());
        assert!(run(&["resize", "/img:1", "1M", "--end", "100", "--yes"]).is_err());
        assert!(
            run(&[
                "recover",
                "/img",
                "--release",
                "--replay-table-only",
                "--yes"
            ])
            .is_err()
        );
        assert!(run(&["recover", "/img", "--discard-replay", "--yes"]).is_err());
    }

    #[test]
    fn flag_form_requires_a_state() {
        assert!(run(&["set", "/img:1", "flag", "2", "--yes"]).is_err());
        assert!(run(&["set", "/img:1", "flag", "2", "maybe", "--yes"]).is_err());
    }
}
