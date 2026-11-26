//! 只读命令的参数解析：形态选择、选项白名单与目标后缀。
//!
//! 解析只产出 [`Request`]；任何不认识、重复或缺失的参数都变成 [`UsageError`]，
//! 由命令层统一以用法错误退出。目标上的分区是否存在不在这里判断。

use diskedit_core::fs::MkfsKind;
use diskedit_core::table::Guid;

use crate::app::observe::{
    Alignment, Command, CopyRequest, MoveRequest, PartitionSelector, Request, SetKey, SetRequest,
    StartSpec, TableFormat, TargetSpec, TypeSelector,
};

/// 用法错误：外层打印到 stderr 并以 2 退出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError {
    pub message: String,
}

impl UsageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 解析一条只读命令。
pub fn parse(args: &[String]) -> Result<Request, UsageError> {
    let Some(command) = args.first() else {
        return Err(UsageError::new("missing command"));
    };
    let rest = &args[1..];
    match command.as_str() {
        "info" => parse_info(rest),
        "inspect" => parse_inspect(rest),
        "records" => parse_records(rest),
        "check" => parse_check(rest),
        "new" => parse_new(rest),
        "add" => parse_add(rest),
        "delete" => parse_delete(rest),
        "set" => parse_set(rest),
        "move" => parse_move(rest),
        "copy" => parse_copy(rest),
        "mkfs" => parse_mkfs(rest),
        "resizefs" => parse_resizefs(rest),
        other => Err(UsageError::new(format!("unknown command `{other}`"))),
    }
}

fn parse_info(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(args, &["--sector-size"], &["--grow"])?;
    let target = one_target(&tokens, "info")?;
    let sector_size = sector_size(&tokens)?;
    let grow = tokens.has("--grow");
    if grow {
        if target.partition.is_none() {
            return Err(UsageError::new("info --grow needs <TARGET>:N"));
        }
    } else if target.partition.is_some() {
        return Err(UsageError::new(
            "info accepts a partition selector only with --grow",
        ));
    }
    let mut request = Request::new(target, Command::Info { grow });
    request = request.with_sector_size(sector_size);
    Ok(request)
}

fn parse_inspect(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(args, &["--identity"], &[])?;
    let target = one_target(&tokens, "inspect")?;
    if target.partition.is_some() {
        return Err(UsageError::new(
            "inspect does not accept a partition selector",
        ));
    }
    Ok(Request::new(target, Command::Inspect).with_identity(identity(&tokens)))
}

fn parse_records(args: &[String]) -> Result<Request, UsageError> {
    let Some(subcommand) = args.first() else {
        return Err(UsageError::new("records needs list, verify or scan"));
    };
    match subcommand.as_str() {
        "list" => {
            let tokens = Tokens::scan(&args[1..], &["--identity"], &[])?;
            let target = optional_target(&tokens)?;
            if let Some(target) = &target {
                if target.partition.is_some() {
                    return Err(UsageError::new("records list does not accept a selector"));
                }
            }
            if target.is_none() && tokens.has("--identity") {
                return Err(UsageError::new(
                    "`--identity` names a target; give TARGET or drop the option",
                ));
            }
            Ok(match target {
                Some(target) => Request::new(target, Command::RecordsList),
                None => Request::without_target(Command::RecordsList),
            }
            .with_identity(identity(&tokens)))
        }
        "verify" => {
            let tokens = Tokens::scan(&args[1..], &["--identity"], &[])?;
            if tokens.positionals.len() != 2 {
                return Err(UsageError::new("records verify needs <TARGET> <ID>"));
            }
            let target = target_spec(&tokens.positionals[0])?;
            if target.partition.is_some() {
                return Err(UsageError::new(
                    "records verify does not accept a partition selector",
                ));
            }
            let token = record_token(&tokens.positionals[1])?;
            Ok(Request::new(target, Command::RecordsVerify { token })
                .with_identity(identity(&tokens)))
        }
        "scan" => {
            let tokens = Tokens::scan(&args[1..], &[], &[])?;
            let target = one_target(&tokens, "records scan")?;
            Ok(Request::new(target, Command::RecordsScan))
        }
        other => Err(UsageError::new(format!(
            "unknown records subcommand `{other}`"
        ))),
    }
}

fn parse_check(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(args, &["--sector-size"], &[])?;
    let target = one_target(&tokens, "check")?;
    let partition = target
        .partition
        .ok_or_else(|| UsageError::new("check needs <TARGET>:N"))?;
    Ok(Request::new(target, Command::Check { partition }).with_sector_size(sector_size(&tokens)?))
}

fn parse_mkfs(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(
        args,
        &["--label", "--identity", "--sector-size"],
        &["--yes"],
    )?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`mkfs` needs --yes"));
    }
    if tokens.positionals.len() != 2 {
        return Err(UsageError::new("mkfs needs <TARGET>:N <FS>"));
    }
    let target = target_spec(&tokens.positionals[0])?;
    let name = &tokens.positionals[1];
    let filesystem = MkfsKind::parse(name).ok_or_else(|| {
        UsageError::new(format!(
            "unknown filesystem `{name}` — use one of ext2, ext3, ext4, xfs, btrfs, f2fs, vfat, exfat, ntfs, swap, lvm2_pv"
        ))
    })?;
    let command = Command::Mkfs {
        partition: target.partition,
        filesystem,
        label: tokens.value("--label").map(str::to_owned),
    };
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_resizefs(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(args, &["--identity", "--sector-size"], &["--yes"])?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`resizefs` needs --yes"));
    }
    if tokens.positionals.is_empty() || tokens.positionals.len() > 2 {
        return Err(UsageError::new(
            "resizefs needs <TARGET>:N [BYTES] or <MOUNTPOINT> [BYTES]",
        ));
    }
    let target = target_spec(&tokens.positionals[0])?;
    if target.partition.is_some() && tokens.positionals.len() > 1 {
        return Err(UsageError::new(
            "a size only applies to the mountpoint form — offline resizefs grows the filesystem into its partition",
        ));
    }
    let bytes = tokens
        .positionals
        .get(1)
        .map(|raw| absolute_size(raw))
        .transpose()?;
    let command = Command::Resizefs {
        partition: target.partition,
        bytes,
    };
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_new(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(
        args,
        &["--table", "--identity", "--sector-size"],
        &["--yes", "--overwrite-data"],
    )?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`new` needs --yes"));
    }
    let target = one_target(&tokens, "new")?;
    if target.partition.is_some() {
        return Err(UsageError::new(
            "new takes a whole target — drop the :N suffix",
        ));
    }
    let table = match tokens.value("--table") {
        None | Some("gpt") => TableFormat::Gpt,
        Some("mbr") => TableFormat::Mbr,
        Some(other) => {
            return Err(UsageError::new(format!(
                "bad value `{other}` for --table (gpt|mbr)"
            )));
        }
    };
    let command = Command::New {
        table,
        overwrite: tokens.has("--overwrite-data"),
    };
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_add(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(
        args,
        &[
            "--align",
            "--start",
            "--part-number",
            "--name",
            "--type-guid",
            "--identity",
            "--sector-size",
        ],
        &["--yes"],
    )?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`add` needs --yes"));
    }
    if tokens.positionals.len() != 2 {
        return Err(UsageError::new("add needs <TARGET> <SIZE>"));
    }
    let target = target_spec(&tokens.positionals[0])?;
    if target.partition.is_some() {
        return Err(UsageError::new(
            "add writes the partition table — drop the :N suffix",
        ));
    }
    let size = absolute_size(&tokens.positionals[1])?;
    let align = match tokens.value("--align") {
        None => Alignment::Default,
        Some("mib") => Alignment::Mib,
        Some("none") => Alignment::None,
        Some(other) => {
            return Err(UsageError::new(format!(
                "bad value `{other}` for --align (mib|none)"
            )));
        }
    };
    let start = tokens.value("--start").map(parse_start).transpose()?;
    let part_number = tokens
        .value("--part-number")
        .map(parse_part_number)
        .transpose()?;
    let type_guid = tokens
        .value("--type-guid")
        .map(parse_type_selector)
        .transpose()?;
    let name = tokens.value("--name").map(str::to_owned);
    let command = Command::Add(crate::app::observe::AddRequest {
        size,
        start,
        align,
        part_number,
        name,
        type_guid,
    });
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_delete(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(args, &["--identity", "--sector-size"], &["--yes"])?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`delete` needs --yes"));
    }
    let target = one_target(&tokens, "delete")?;
    let Some(partition) = target.partition else {
        return Err(UsageError::new(
            "`delete` needs a partition: write <TARGET>:N",
        ));
    };
    Ok(Request::new(target, Command::Delete { partition })
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_set(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(
        args,
        &["--identity", "--sector-size"],
        &["--yes", "--random"],
    )?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`set` needs --yes"));
    }
    if tokens.positionals.len() < 2 {
        return Err(UsageError::new(
            "set needs <TARGET>:N <PROPERTY> [VALUE] [on|off]",
        ));
    }
    let target = target_spec(&tokens.positionals[0])?;
    let Some(partition) = target.partition else {
        return Err(UsageError::new("`set` needs a partition: write <TARGET>:N"));
    };
    let Some(key) = SetKey::from_text(&tokens.positionals[1]) else {
        return Err(UsageError::new(format!(
            "unknown property `{}` for `set` — use name, label, uuid, flag or type",
            tokens.positionals[1]
        )));
    };
    let extras = &tokens.positionals[2..];
    let (value, state) = match key {
        SetKey::Name | SetKey::Label | SetKey::Type => {
            if extras.is_empty() || extras.len() > 2 {
                return Err(UsageError::new(format!(
                    "`set {}` needs a value",
                    key.as_str()
                )));
            }
            (
                extras[0].clone(),
                extras.get(1).cloned().unwrap_or_default(),
            )
        }
        SetKey::Uuid => {
            if extras.len() > 1 {
                return Err(UsageError::new("`set uuid` takes at most one value"));
            }
            (extras.first().cloned().unwrap_or_default(), String::new())
        }
        SetKey::Flag => {
            if extras.len() != 2 {
                return Err(UsageError::new("`set flag` needs <NAME> <on|off>"));
            }
            (extras[0].clone(), extras[1].clone())
        }
    };
    if tokens.has("--random") && key != SetKey::Uuid {
        return Err(UsageError::new("`--random` only applies to `set uuid`"));
    }
    if key == SetKey::Uuid && tokens.has("--random") && !value.is_empty() {
        return Err(UsageError::new(
            "`set uuid` takes a value or --random, not both",
        ));
    }
    let command = Command::Set(SetRequest {
        partition,
        key,
        value,
        state,
        random: tokens.has("--random"),
    });
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_move(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(
        args,
        &[
            "--align",
            "--start",
            "--chunk",
            "--identity",
            "--sector-size",
        ],
        &["--yes"],
    )?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`move` needs --yes"));
    }
    let target = one_target(&tokens, "move")?;
    let Some(partition) = target.partition else {
        return Err(UsageError::new(
            "`move` needs a partition: write <TARGET>:N",
        ));
    };
    let Some(raw) = tokens.value("--start") else {
        return Err(UsageError::new("`move` needs --start <LBA|end|align>"));
    };
    let command = Command::Move(MoveRequest {
        partition,
        start: parse_start(raw)?,
        align: alignment(&tokens)?,
        chunk: chunk_bytes(&tokens)?,
    });
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

fn parse_copy(args: &[String]) -> Result<Request, UsageError> {
    let tokens = Tokens::scan(
        args,
        &[
            "--align",
            "--start",
            "--chunk",
            "--name",
            "--identity",
            "--sector-size",
        ],
        &["--yes", "--keep-partuuid"],
    )?;
    if !tokens.has("--yes") {
        return Err(UsageError::new("`copy` needs --yes"));
    }
    let target = one_target(&tokens, "copy")?;
    let Some(partition) = target.partition else {
        return Err(UsageError::new(
            "`copy` needs a partition: write <TARGET>:N",
        ));
    };
    let Some(raw) = tokens.value("--start") else {
        return Err(UsageError::new("`copy` needs --start <LBA|end>"));
    };
    let command = Command::Copy(CopyRequest {
        partition,
        start: parse_start(raw)?,
        align: alignment(&tokens)?,
        chunk: chunk_bytes(&tokens)?,
        name: tokens.value("--name").map(str::to_owned),
        keep_partuuid: tokens.has("--keep-partuuid"),
    });
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens))
        .with_confirmation(true))
}

/// `--align` 的取值；未给出时按不对齐。
fn alignment(tokens: &Tokens) -> Result<Alignment, UsageError> {
    match tokens.value("--align") {
        None => Ok(Alignment::Default),
        Some("mib") => Ok(Alignment::Mib),
        Some("none") => Ok(Alignment::None),
        Some(other) => Err(UsageError::new(format!(
            "bad value `{other}` for --align (mib|none)"
        ))),
    }
}

/// `--chunk` 以 MiB 计，取值 1..=64；返回字节数。
fn chunk_bytes(tokens: &Tokens) -> Result<Option<u64>, UsageError> {
    let Some(raw) = tokens.value("--chunk") else {
        return Ok(None);
    };
    let mib: u64 = raw
        .parse()
        .map_err(|_| UsageError::new(format!("`{raw}` is not a chunk size in MiB")))?;
    if !(1..=64).contains(&mib) {
        return Err(UsageError::new("--chunk must be in 1..=64 MiB"));
    }
    Ok(Some(mib << 20))
}

fn parse_start(raw: &str) -> Result<StartSpec, UsageError> {
    match raw {
        "end" => Ok(StartSpec::End),
        "align" => Ok(StartSpec::Align),
        _ => raw
            .parse::<u64>()
            .map(StartSpec::Lba)
            .map_err(|_| UsageError::new(format!("`{raw}` is not an LBA or end|align"))),
    }
}

fn parse_part_number(raw: &str) -> Result<u32, UsageError> {
    let number: u32 = raw
        .parse()
        .map_err(|_| UsageError::new(format!("`{raw}` is not a partition number")))?;
    if number == 0 {
        return Err(UsageError::new("partition numbers start at 1"));
    }
    Ok(number)
}

fn parse_type_selector(raw: &str) -> Result<TypeSelector, UsageError> {
    if let Some(byte) = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .filter(|digits| !digits.is_empty() && digits.len() <= 2)
        .and_then(|digits| u8::from_str_radix(digits, 16).ok())
    {
        return Ok(TypeSelector::Mbr(byte));
    }
    Guid::parse(raw)
        .map(TypeSelector::Guid)
        .ok_or_else(|| UsageError::new(format!("`{raw}` is not a GUID or 0xNN type byte")))
}

/// `add` 只接受绝对字节数；相对写法（+32M、-32M、32%）没有可参照的前值。
fn absolute_size(raw: &str) -> Result<u64, UsageError> {
    if raw.starts_with('+') || raw.starts_with('-') || raw.ends_with('%') {
        return Err(UsageError::new(
            "add needs an absolute SIZE (e.g. 512M) — relative forms are not accepted",
        ));
    }
    let (value, unit) = match raw.as_bytes().last() {
        Some(b'k' | b'K') => (&raw[..raw.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&raw[..raw.len() - 1], 1u64 << 20),
        Some(b'g' | b'G') => (&raw[..raw.len() - 1], 1u64 << 30),
        Some(b't' | b'T') => (&raw[..raw.len() - 1], 1u64 << 40),
        _ => (raw, 1),
    };
    let Some(bytes) = decimal(value).and_then(|(num, denom)| {
        (num as u128)
            .checked_mul(unit as u128)?
            .checked_div(denom as u128)
            .filter(|bytes| *bytes <= u64::MAX as u128)
            .map(|bytes| bytes as u64)
    }) else {
        return Err(UsageError::new(format!("`{raw}` is not a size")));
    };
    Ok(bytes)
}

fn decimal(text: &str) -> Option<(u64, u64)> {
    if text.is_empty() || text.ends_with('.') {
        return None;
    }
    let (int_part, frac_part) = text.split_once('.').unwrap_or((text, ""));
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|byte| byte.is_ascii_digit())
        || !frac_part.bytes().all(|byte| byte.is_ascii_digit())
        || frac_part.len() > 18
    {
        return None;
    }
    let denom = 10u64.checked_pow(frac_part.len() as u32)?;
    let int: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    let frac: u64 = if frac_part.is_empty() {
        0
    } else {
        frac_part.parse().ok()?
    };
    let num = (u128::from(int))
        .checked_mul(u128::from(denom))?
        .checked_add(u128::from(frac))
        .filter(|num| *num <= u128::from(u64::MAX))?;
    Some((num as u64, denom))
}

fn one_target(tokens: &Tokens, command: &str) -> Result<TargetSpec, UsageError> {
    if tokens.positionals.len() != 1 {
        return Err(UsageError::new(format!(
            "{command} needs exactly one target"
        )));
    }
    target_spec(&tokens.positionals[0])
}

fn optional_target(tokens: &Tokens) -> Result<Option<TargetSpec>, UsageError> {
    match tokens.positionals.len() {
        0 => Ok(None),
        1 => Ok(Some(target_spec(&tokens.positionals[0])?)),
        _ => Err(UsageError::new("too many targets")),
    }
}

fn target_spec(raw: &str) -> Result<TargetSpec, UsageError> {
    let Some((path, suffix)) = raw.rsplit_once(':') else {
        return Ok(TargetSpec::new(raw, None));
    };
    if path.is_empty() {
        return Err(UsageError::new("target path is empty"));
    }
    if suffix == "last" {
        return Ok(TargetSpec::new(path, Some(PartitionSelector::Last)));
    }
    if !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        let number: u32 = suffix
            .parse()
            .map_err(|_| UsageError::new(format!("partition number `{suffix}` is too large")))?;
        if number == 0 {
            return Err(UsageError::new("partition numbers start at 1"));
        }
        return Ok(TargetSpec::new(
            path,
            Some(PartitionSelector::Number(number)),
        ));
    }
    Ok(TargetSpec::new(raw, None))
}

fn record_token(raw: &str) -> Result<String, UsageError> {
    if raw.len() == 32 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err(UsageError::new(
            "record id must be 32 hexadecimal characters",
        ))
    }
}

fn sector_size(tokens: &Tokens) -> Result<Option<u64>, UsageError> {
    let Some(raw) = tokens.value("--sector-size") else {
        return Ok(None);
    };
    let size: u64 = raw
        .parse()
        .map_err(|_| UsageError::new(format!("`{raw}` is not a sector size")))?;
    if size < 512 || !size.is_power_of_two() {
        return Err(UsageError::new(
            "sector size must be a power of two of at least 512",
        ));
    }
    Ok(Some(size))
}

fn identity(tokens: &Tokens) -> Option<String> {
    tokens.value("--identity").map(str::to_owned)
}

/// 一次扫描得到的原始参数。
struct Tokens {
    positionals: Vec<String>,
    options: Vec<(String, String)>,
}

impl Tokens {
    fn scan(
        args: &[String],
        value_flags: &[&str],
        bool_flags: &[&str],
    ) -> Result<Self, UsageError> {
        let mut positionals = Vec::new();
        let mut options: Vec<(String, String)> = Vec::new();
        let mut index = 0;
        let mut literal_only = false;
        while index < args.len() {
            let argument = &args[index];
            if literal_only {
                positionals.push(argument.clone());
                index += 1;
                continue;
            }
            if argument == "--" {
                literal_only = true;
                index += 1;
                continue;
            }
            if let Some(flag) = argument.strip_prefix("--") {
                let (name, inline) = match flag.split_once('=') {
                    Some((name, value)) => (format!("--{name}"), Some(value.to_owned())),
                    None => (argument.clone(), None),
                };
                if bool_flags.contains(&name.as_str()) {
                    if inline.is_some() {
                        return Err(UsageError::new(format!("{name} takes no value")));
                    }
                    push_option(&mut options, name, String::new())?;
                } else if value_flags.contains(&name.as_str()) {
                    let value = match inline {
                        Some(value) => value,
                        None => {
                            index += 1;
                            args.get(index)
                                .cloned()
                                .ok_or_else(|| UsageError::new(format!("{name} needs a value")))?
                        }
                    };
                    push_option(&mut options, name, value)?;
                } else {
                    return Err(UsageError::new(format!("unknown option `{name}`")));
                }
                index += 1;
                continue;
            }
            positionals.push(argument.clone());
            index += 1;
        }
        Ok(Self {
            positionals,
            options,
        })
    }

    fn has(&self, name: &str) -> bool {
        self.options.iter().any(|(option, _)| option == name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(option, _)| option == name)
            .map(|(_, value)| value.as_str())
    }
}

fn push_option(
    options: &mut Vec<(String, String)>,
    name: String,
    value: String,
) -> Result<(), UsageError> {
    if options.iter().any(|(existing, _)| *existing == name) {
        return Err(UsageError::new(format!("{name} given more than once")));
    }
    options.push((name, value));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<Request, UsageError> {
        let args: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        parse(&args)
    }

    #[test]
    fn info_accepts_whole_target_and_sector_size() {
        let request = parse_words(&["info", "disk.img", "--sector-size", "4096"]).expect("合法");
        assert_eq!(request.sector_size(), Some(4096));
        assert!(matches!(request.command(), Command::Info { grow: false }));
        assert!(request.partition_selector().is_none());
    }

    #[test]
    fn info_grow_needs_a_partition() {
        assert!(parse_words(&["info", "disk.img", "--grow"]).is_err());
        let request = parse_words(&["info", "disk.img:last", "--grow"]).expect("合法");
        assert_eq!(request.partition_selector(), Some(PartitionSelector::Last));
        assert!(matches!(request.command(), Command::Info { grow: true }));
    }

    #[test]
    fn info_rejects_a_selector_without_grow() {
        assert!(parse_words(&["info", "disk.img:1"]).is_err());
    }

    #[test]
    fn sector_size_must_be_a_large_power_of_two() {
        for bad in ["0", "123", "256"] {
            assert!(parse_words(&["info", "disk.img", "--sector-size", bad]).is_err());
        }
        for good in ["512", "1024", "4096"] {
            assert!(parse_words(&["info", "disk.img", "--sector-size", good]).is_ok());
        }
    }

    #[test]
    fn target_suffix_keeps_unparseable_colons_in_the_path() {
        let request = parse_words(&["info", "weird:name.img"]).expect("合法");
        assert_eq!(
            request.target().expect("目标").to_str(),
            Some("weird:name.img")
        );
        assert!(parse_words(&["info", "disk.img:0"]).is_err());
    }

    #[test]
    fn records_verify_requires_a_hex_id() {
        assert!(parse_words(&["records", "verify", "disk.img", "zz"]).is_err());
        let request = parse_words(&[
            "records",
            "verify",
            "disk.img",
            "AABBCCDDEEFF00112233445566778899",
        ])
        .expect("合法");
        assert!(matches!(
            request.command(),
            Command::RecordsVerify { token } if token == "aabbccddeeff00112233445566778899"
        ));
    }

    #[test]
    fn unknown_options_and_duplicates_are_usage_errors() {
        assert!(parse_words(&["info", "disk.img", "--bogus"]).is_err());
        assert!(
            parse_words(&[
                "info",
                "disk.img",
                "--sector-size",
                "512",
                "--sector-size",
                "4096"
            ])
            .is_err()
        );
    }

    #[test]
    fn check_needs_a_partition() {
        assert!(parse_words(&["check", "disk.img"]).is_err());
        assert!(parse_words(&["check", "disk.img:2"]).is_ok());
    }

    #[test]
    fn move_needs_a_partition_a_start_and_yes() {
        assert!(parse_words(&["move", "disk.img:1", "--start", "5000"]).is_err());
        assert!(parse_words(&["move", "disk.img", "--start", "5000", "--yes"]).is_err());
        assert!(parse_words(&["move", "disk.img:1", "--yes"]).is_err());
        let request =
            parse_words(&["move", "disk.img:2", "--start", "5000", "--yes"]).expect("合法");
        assert!(matches!(
            request.command(),
            Command::Move(m) if m.start == StartSpec::Lba(5000)
        ));
    }

    #[test]
    fn chunk_is_mebibytes_in_range() {
        for bad in ["0", "65", "x"] {
            assert!(
                parse_words(&[
                    "move",
                    "disk.img:1",
                    "--start",
                    "5000",
                    "--chunk",
                    bad,
                    "--yes"
                ])
                .is_err()
            );
        }
        let request = parse_words(&[
            "move",
            "disk.img:1",
            "--start",
            "5000",
            "--chunk",
            "8",
            "--yes",
        ])
        .expect("合法");
        assert!(matches!(request.command(), Command::Move(m) if m.chunk == Some(8 << 20)));
    }

    #[test]
    fn copy_parses_name_and_keep_partuuid() {
        let request = parse_words(&[
            "copy",
            "disk.img:1",
            "--start",
            "5000",
            "--name",
            "clone",
            "--keep-partuuid",
            "--yes",
        ])
        .expect("合法");
        match request.command() {
            Command::Copy(copy) => {
                assert_eq!(copy.name.as_deref(), Some("clone"));
                assert!(copy.keep_partuuid);
            }
            Command::Info { .. }
            | Command::Inspect
            | Command::RecordsList
            | Command::RecordsVerify { .. }
            | Command::RecordsScan
            | Command::Resize(_)
            | Command::Move(_)
            | Command::Delete { .. }
            | Command::Set(_)
            | Command::Check { .. }
            | Command::Mkfs { .. }
            | Command::Resizefs { .. }
            | Command::Recover(_)
            | Command::New { .. }
            | Command::Add(_) => panic!("expected copy"),
        }
    }
}
