//! 命令行层：声明式命令定义、`argv → Request`、帮助与用法错误。
//!
//! 本层只把字面量解析成请求；观察、判定、执行与报告各由下层承担。用法错误一律
//! 落退出码 [`report::exit::USAGE`]，且不打开目标。

mod parse;
pub(crate) mod spec;

use std::ffi::OsString;

use crate::app::decide::{Decision, Refusal, confirm, decide};
use crate::app::execute;
use crate::app::observe::{Command, PartitionSelector, Request, Snapshot, TargetSpec, observe};
use crate::app::report::{exit, json, text};
use crate::platform::{self, Platform, PlatformError};

use parse::Invocation;

/// 声明过的全部入口名，按声明顺序。
pub fn declared_entries() -> impl Iterator<Item = &'static str> {
    spec::SPECS.iter().map(|entry| entry.name)
}

/// 解析参数并返回退出码。
pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> u8 {
    let args: Vec<OsString> = args.into_iter().collect();
    if args.get(1).is_some_and(|token| token == "help") {
        return help(&args[2..]);
    }
    let invocation = match parse::parse(args) {
        Ok(invocation) => invocation,
        Err(message) if message.is_empty() => return 0,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("run `diskedit --help` for usage.");
            return exit::USAGE;
        }
    };
    run(invocation)
}

/// `diskedit help [CMD]`：没有名字或名字未知时打印全局帮助。
fn help(rest: &[OsString]) -> u8 {
    let query: Vec<String> = rest
        .iter()
        .map(|token| token.to_string_lossy().into_owned())
        .collect();
    if query.is_empty() {
        spec::print_help();
        return exit::SUCCESS;
    }
    let name = query.join(" ");
    if spec::print_command(&name) {
        return exit::SUCCESS;
    }
    spec::print_help();
    exit::USAGE
}

fn run(invocation: Invocation) -> u8 {
    let platform = match platform::open() {
        Ok(platform) => platform,
        Err(error) => return platform_failure(&error),
    };
    let invocation = match resolve_mount_form(invocation, &*platform) {
        Ok(invocation) => invocation,
        Err(code) => return code,
    };
    if let Some(code) = reject_identity_on_identified(&invocation.request, &*platform) {
        return code;
    }
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
                println!("dry-run: plan generated, nothing written to target");
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
                eprintln!("write command is missing its target");
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

/// `--identity` 只对自动身份不可得的目标成立；目标自身已能稳定识别时明确拒绝，
/// 不让声明名悄悄覆盖设备事实。
fn reject_identity_on_identified(request: &Request, platform: &dyn Platform) -> Option<u8> {
    let name = request.identity()?;
    let target = request.target()?;
    match platform.fingerprint(target) {
        Ok(fingerprint)
            if fingerprint.strength() == diskedit_core::fingerprint::Strength::Strong =>
        {
            eprintln!(
                "--identity is only for block devices without an automatic identity; {} already has a stable identity, declared name {name} does not apply",
                target.display()
            );
            Some(exit::USAGE)
        }
        _ => None,
    }
}

/// 挂载点形态的 `resizefs`：先把目录反查成整盘目标与分区号，再走同一套离线判定。
///
/// 反查失败即请求与现场不符（目录未挂载、设备不是分区、整盘或逻辑卷挂载），
/// 按事实类别给出退出码，不把挂载点当可写目标继续。
fn resolve_mount_form(
    mut invocation: Invocation,
    platform: &dyn Platform,
) -> Result<Invocation, u8> {
    let Command::Resizefs {
        partition: None,
        bytes,
        mount: false,
    } = invocation.request.command().clone()
    else {
        return Ok(invocation);
    };
    let Some(target) = invocation.request.target_spec().cloned() else {
        return Ok(invocation);
    };
    let location = match platform.mount_location(&target.path) {
        Ok(location) => location,
        Err(error) => {
            eprintln!("{error}");
            return Err(match error {
                PlatformError::Parse { .. } => exit::REFUSED,
                PlatformError::NotSupported { .. } | PlatformError::UnsupportedFs { .. } => {
                    exit::UNSUPPORTED
                }
                PlatformError::Io { .. }
                | PlatformError::Busy { .. }
                | PlatformError::Denied { .. }
                | PlatformError::Tools { .. } => exit::ENVIRONMENT,
            });
        }
    };
    let selector = PartitionSelector::Number(location.partition);
    let command = Command::Resizefs {
        partition: Some(selector),
        bytes,
        mount: true,
    };
    invocation.request = invocation
        .request
        .with_target(TargetSpec::new(location.target, Some(selector)))
        .with_command(command);
    Ok(invocation)
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
            render_check(request, snapshot, platform, *partition, target.as_deref())
        }
        Command::RecordsList => render_records(request, snapshot, platform),
        Command::RecordsVerify { token } => {
            render_records_verify(request, snapshot, platform, token)
        }
        Command::RecordsScan => render_records_scan(request, platform),
        Command::Resize(_)
        | Command::Move(_)
        | Command::Copy(_)
        | Command::Delete { .. }
        | Command::Set { .. }
        | Command::Mkfs { .. }
        | Command::Resizefs { .. }
        | Command::Recover(_)
        | Command::New { .. }
        | Command::Add(_) => {
            eprintln!("this command has no read-only render path");
            exit::INTERNAL
        }
    }
}

fn render_check(
    request: &Request,
    snapshot: &Snapshot,
    platform: &dyn Platform,
    selector: PartitionSelector,
    target: Option<&str>,
) -> u8 {
    let Some(number) = resolve(snapshot, selector) else {
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
    let Some(path) = request.target() else {
        eprintln!("check requires a target");
        return exit::INTERNAL;
    };
    let Some(sector) = snapshot.disk.sector_size.value().copied() else {
        eprintln!("sector size unknown, cannot locate partition range");
        return exit::INTERNAL;
    };
    let Some(content) = facts.content.value().copied() else {
        eprintln!("partition content unknown, cannot choose a checker");
        return exit::INTERNAL;
    };
    let offset = match facts.partition.start_lba.checked_mul(sector) {
        Some(offset) => offset,
        None => {
            eprintln!("partition start exceeds the addressable limit");
            return exit::INTERNAL;
        }
    };
    let region = match diskedit_core::region::Region::sized(
        offset,
        facts.partition.sectors.saturating_mul(sector),
    ) {
        Ok(region) => region,
        Err(error) => {
            eprintln!("invalid partition range: {error:?}");
            return exit::INTERNAL;
        }
    };
    match execute::check::run(platform, path, number, region, content) {
        Ok(report) => {
            println!(
                "{}",
                json::check::render(target.unwrap_or(""), facts, &report)
            );
            exit::SUCCESS
        }
        Err(execute::ExecError::Check(why)) => {
            eprintln!("{why}");
            exit::REFUSED
        }
        Err(error) => exec_failure(&error),
    }
}

fn render_records(request: &Request, snapshot: &Snapshot, platform: &dyn Platform) -> u8 {
    let Some(target) = request.target() else {
        println!("{}", json::records::render_unavailable());
        return exit::SUCCESS;
    };
    let Some(identity) = strong_identity(snapshot) else {
        return identity_unprovable();
    };
    match execute::records::list(platform, target, identity) {
        Ok(rows) => {
            println!("{}", json::records::render(&rows));
            exit::SUCCESS
        }
        Err(error) => exec_failure(&error),
    }
}

fn render_records_verify(
    request: &Request,
    snapshot: &Snapshot,
    platform: &dyn Platform,
    token: &str,
) -> u8 {
    let Some(target) = request.target() else {
        eprintln!("records verify requires a target to locate the record directory");
        return exit::REFUSED;
    };
    let Some(identity) = strong_identity(snapshot) else {
        return identity_unprovable();
    };
    match execute::records::verify(platform, target, identity, token) {
        Ok(report) => {
            use execute::records::VerifyVerdict;
            let verdict = report.verdict;
            println!("{}", json::records::render_verify(&report));
            match verdict {
                VerifyVerdict::Valid | VerifyVerdict::Unfinished => exit::SUCCESS,
                VerifyVerdict::Unusable | VerifyVerdict::NotFound | VerifyVerdict::NotUnique => {
                    exit::REFUSED
                }
            }
        }
        Err(error) => exec_failure(&error),
    }
}

fn render_records_scan(request: &Request, platform: &dyn Platform) -> u8 {
    let Some(root) = request.target() else {
        eprintln!("records scan requires a container root");
        return exit::REFUSED;
    };
    match execute::records::scan(platform, root) {
        Ok(rows) => {
            println!(
                "{}",
                json::records::render_scan(&root.display().to_string(), &rows)
            );
            exit::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            exit::REFUSED
        }
    }
}

fn strong_identity(snapshot: &Snapshot) -> Option<diskedit_core::fingerprint::Fingerprint> {
    snapshot.identity.value().copied().filter(|fingerprint| {
        fingerprint.strength() == diskedit_core::fingerprint::Strength::Strong
    })
}

fn identity_unprovable() -> u8 {
    eprintln!(
        "{}",
        text::refusal(&Refusal::IdentityUnprovable {
            why: "cannot read target identity, cannot locate the record directory".to_owned(),
        })
    );
    exit::UNSUPPORTED
}

fn exec_failure(error: &execute::ExecError) -> u8 {
    match error {
        execute::ExecError::Platform(error) => platform_failure(error),
        execute::ExecError::Record(_)
        | execute::ExecError::IdentityChanged(_)
        | execute::ExecError::Unsupported(_)
        | execute::ExecError::InvalidPlan(_)
        | execute::ExecError::Io(_)
        | execute::ExecError::Postcondition(_)
        | execute::ExecError::Check(_)
        | execute::ExecError::Injected(_) => {
            eprintln!("{error}");
            exit::ENVIRONMENT
        }
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
