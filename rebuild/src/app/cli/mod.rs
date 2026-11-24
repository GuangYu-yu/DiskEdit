//! 命令行：声明命令面、渲染帮助与用法错误。
//!
//! 命令面只有一张表（[`declare`]）；帮助与门禁导出都读它。S0 固定声明与进程级
//! 出口（帮助、版本、用法错误），具体命令的解析与判定按纵切接入。

pub mod declare;

use std::ffi::OsString;

use crate::app::report::exit;

pub use declare::declared_entries;

/// 运行一次命令行调用，返回退出码。
pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> u8 {
    let argv: Vec<String> = args
        .into_iter()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();

    if argv.first().is_some_and(|first| first == "help") {
        return help_topic(&argv[1..]);
    }
    if let Some(position) = argv
        .iter()
        .position(|argument| argument == "--help" || argument == "-h")
    {
        let mut rest = argv.clone();
        rest.remove(position);
        return help_topic(&rest);
    }
    if argv.len() == 1 && (argv[0] == "--version" || argv[0] == "-V") {
        println!("diskedit {}", env!("CARGO_PKG_VERSION"));
        return exit::SUCCESS;
    }
    usage()
}

/// 一个帮助主题：空为顶层帮助，命令名可含空格（`records list`）。
fn help_topic(words: &[String]) -> u8 {
    if words.is_empty() {
        print_help();
        return exit::SUCCESS;
    }
    let name = words.join(" ");
    match declare::find(&name) {
        Some(command) => {
            print_command_help(command);
            exit::SUCCESS
        }
        None => usage(),
    }
}

/// 顶层帮助：按分组列出入口与全部形态。
fn print_help() {
    println!("diskedit - disk / image editor");
    for group in [declare::Group::Everyday, declare::Group::LowLevel] {
        println!();
        println!(
            "{}",
            match group {
                declare::Group::Everyday => "Commands:",
                declare::Group::LowLevel => "Low-level:",
            }
        );
        for command in declare::COMMANDS
            .iter()
            .filter(|command| command.group == group)
        {
            println!("  {:14} {}", command.name, command.summary);
            for variant in command.variants {
                println!("      diskedit {} {}", command.name, variant.syntax);
            }
        }
    }
}

/// 单个命令的帮助；与顶层帮助读同一张声明表。
fn print_command_help(command: &declare::CommandDef) {
    for variant in command.variants {
        println!("diskedit {} {}", command.name, variant.syntax);
    }
    println!();
    println!("  {}", command.summary);
    let mut options: Vec<&str> = command.common_options.to_vec();
    for variant in command.variants {
        for option in variant.options {
            if !options.contains(option) {
                options.push(option);
            }
        }
    }
    options.sort_unstable();
    if !options.is_empty() {
        println!();
        println!("options:");
        for option in options {
            println!("    {option}");
        }
    }
}

/// 用法错误：打到 stderr，退出 2。
fn usage() -> u8 {
    eprintln!("diskedit - disk / image editor");
    for command in declare::COMMANDS {
        eprintln!("  {:14} {}", command.name, command.summary);
    }
    exit::USAGE
}
