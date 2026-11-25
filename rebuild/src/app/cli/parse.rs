//! 只读命令的参数解析：形态选择、选项白名单与目标后缀。
//!
//! 解析只产出 [`Request`]；任何不认识、重复或缺失的参数都变成 [`UsageError`]，
//! 由命令层统一以用法错误退出。目标上的分区是否存在不在这里判断。

use crate::app::observe::{Command, PartitionSelector, Request, TableFormat, TargetSpec};

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
        confirmed: tokens.has("--yes"),
    };
    Ok(Request::new(target, command)
        .with_sector_size(sector_size(&tokens)?)
        .with_identity(identity(&tokens)))
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
}
