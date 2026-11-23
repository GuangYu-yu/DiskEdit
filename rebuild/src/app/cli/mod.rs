//! 命令行层：声明式命令定义、`argv → Request`、帮助与用法错误。
//!
//! 本层只把字面量解析成请求；观察、判定、执行与报告各由下层承担。用法错误一律
//! 落退出码 [`report::exit::USAGE`]，且不打开目标。

mod parse;
pub(crate) mod spec;

use std::ffi::OsString;

use crate::app::decide::{Decision, Refusal, confirm, decide};
use crate::app::execute;
use crate::app::observe::{Command, PartitionSelector, Request, Snapshot, observe};
use crate::app::report::{exit, json, text};
use crate::platform::{self, Platform, PlatformError};

use parse::Invocation;

/// 声明过的全部入口名，按声明顺序。
pub fn declared_entries() -> impl Iterator<Item = &'static str> {
    spec::SPECS.iter().map(|entry| entry.name)
}

/// 解析参数并返回退出码。
pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> u8 {
    let invocation = match parse::parse(args) {
        Ok(invocation) => invocation,
        Err(message) if message.is_empty() => return 0,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("运行 `diskedit --help` 查看用法。");
            return exit::USAGE;
        }
    };
    run(invocation)
}

fn run(invocation: Invocation) -> u8 {
    let platform = match platform::open() {
        Ok(platform) => platform,
        Err(error) => return platform_failure(&error),
    };
    let snapshot = observe(&invocation.request, &*platform);
    match decide(&invocation.request, &snapshot) {
        Decision::Refuse { refusal } => {
            eprintln!("{}", text::refusal(&refusal));
            exit::exit_of_refusal(&refusal)
        }
        Decision::Plan { plan } => {
            if plan.identity().is_none() {
                return render_read_only(&invocation.request, &snapshot, &*platform);
            }
            if invocation.dry_run {
                println!("dry-run：计划已生成，未写入目标");
                return exit::SUCCESS;
            }
            let confirmed = match confirm(plan, invocation.yes, &snapshot.identity) {
                Ok(confirmed) => confirmed,
                Err(refusal) => {
                    eprintln!("{}", text::refusal(&refusal));
                    return exit::exit_of_refusal(&refusal);
                }
            };
            let Some(target) = invocation.request.target() else {
                eprintln!("写命令缺少目标");
                return exit::INTERNAL;
            };
            let outcome =
                execute::execute(&confirmed, &*platform, target, snapshot.generation.get());
            let label = target.display().to_string();
            println!("{}", text::outcome(&outcome, &label, &label));
            exit::exit_of(&outcome)
        }
    }
}

/// 只读命令直接渲染快照，不进入执行。
fn render_read_only(request: &Request, snapshot: &Snapshot, platform: &dyn Platform) -> u8 {
    let target = request.target().map(|path| path.display().to_string());
    match request.command() {
        Command::Info => {
            println!(
                "{}",
                json::info::render(target.as_deref().unwrap_or(""), snapshot)
            );
            exit::SUCCESS
        }
        Command::Inspect => {
            println!(
                "{}",
                json::inspect::render(target.as_deref().unwrap_or(""), snapshot)
            );
            exit::SUCCESS
        }
        Command::Check { partition } => {
            let Some(number) = resolve(snapshot, *partition) else {
                eprintln!(
                    "{}",
                    text::refusal(&Refusal::PartitionMissing { number: 0 })
                );
                return exit::REFUSED;
            };
            let Some(facts) = snapshot.partition(number) else {
                eprintln!("{}", text::refusal(&Refusal::PartitionMissing { number }));
                return exit::REFUSED;
            };
            println!(
                "{}",
                json::check::render(target.as_deref().unwrap_or(""), facts)
            );
            exit::SUCCESS
        }
        Command::RecordsList => render_records(request, snapshot, platform),
        Command::RecordsVerify { .. }
        | Command::RecordsScan
        | Command::Resize(_)
        | Command::Move(_)
        | Command::Copy(_)
        | Command::Delete { .. }
        | Command::Set { .. }
        | Command::Mkfs { .. }
        | Command::Resizefs { .. }
        | Command::Recover(_)
        | Command::New { .. }
        | Command::Add(_) => {
            eprintln!("该命令没有只读渲染通道");
            exit::INTERNAL
        }
    }
}

fn render_records(request: &Request, snapshot: &Snapshot, platform: &dyn Platform) -> u8 {
    let Some(target) = request.target() else {
        eprintln!("records list 需要目标才能定位记录目录");
        return exit::REFUSED;
    };
    let Some(identity) = snapshot.identity.value().copied() else {
        eprintln!(
            "{}",
            text::refusal(&Refusal::IdentityUnprovable {
                why: "目标身份读不出，无法定位记录目录".to_owned(),
            })
        );
        return exit::UNSUPPORTED;
    };
    match platform
        .records(target, identity)
        .and_then(|store| store.entries())
    {
        Ok(entries) => {
            println!("{}", json::records::render(&entries));
            exit::SUCCESS
        }
        Err(error) => platform_failure(&error),
    }
}

fn resolve(snapshot: &Snapshot, selector: PartitionSelector) -> Option<u32> {
    match selector {
        PartitionSelector::Number(number) => Some(number),
        PartitionSelector::Last => snapshot
            .contents()?
            .iter()
            .map(|facts| facts.partition.number)
            .max(),
    }
}

fn platform_failure(error: &PlatformError) -> u8 {
    eprintln!("{error}");
    if error.io_kind().is_some() {
        exit::ENVIRONMENT
    } else {
        exit::UNSUPPORTED
    }
}
