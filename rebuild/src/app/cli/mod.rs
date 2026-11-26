//! 命令行：声明命令面、渲染帮助、解析用法并驱动一次只读判定。
//!
//! 命令面只有一张表（[`declare`]）；帮助与门禁导出都读它。进程级出口（帮助、
//! 版本、用法错误）在这里固定；具体命令的解析、观察、判定与渲染按纵切接入。

pub mod declare;
pub mod parse;

use std::ffi::OsString;

use crate::app::decide::{self, Decision};
use crate::app::execute;
use crate::app::observe::{self, Command};
use crate::app::report::{self, exit};
use crate::platform::{self, Platform};

pub use declare::declared_entries;

/// 运行一次命令行调用，返回退出码。宿主端口在这里选定。
pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> u8 {
    dispatch_with(&platform::host(), args)
}

/// 以给定端口运行一次命令行调用。测试用它注入假端口，生产路径只经 [`dispatch`]。
pub fn dispatch_with(platform: &dyn Platform, args: impl IntoIterator<Item = OsString>) -> u8 {
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

    match parse::parse(&argv) {
        Ok(request) => run(platform, &request),
        Err(error) => {
            eprintln!("diskedit: {}", error.message);
            usage()
        }
    }
}

/// 执行一条已解析的请求。
fn run(platform: &dyn Platform, request: &observe::Request) -> u8 {
    let decision = match request.command() {
        Command::RecordsScan => return scan(platform, request),
        Command::RecordsList if request.target_spec().is_none() => {
            decide::records::list(None, platform.state_root())
        }
        Command::Info { grow } => match observe_or_exit(platform, request, observe::capture) {
            Ok(snapshot) => decide::info::decide(&snapshot, request, *grow),
            Err(code) => return code,
        },
        Command::Check { partition } => {
            match observe_or_exit(platform, request, observe::capture) {
                Ok(snapshot) => decide::check::decide(&snapshot, *partition),
                Err(code) => return code,
            }
        }
        Command::Inspect => match observe_or_exit(platform, request, observe::capture_inspect) {
            Ok(snapshot) => decide::inspect::decide(&snapshot),
            Err(code) => return code,
        },
        Command::RecordsList => {
            match observe_or_exit(platform, request, observe::capture_records) {
                Ok(snapshot) => decide::records::list(Some(&snapshot), platform.state_root()),
                Err(code) => return code,
            }
        }
        Command::RecordsVerify { token } => {
            match observe_or_exit(platform, request, observe::capture_records) {
                Ok(snapshot) => decide::records::verify(&snapshot, token),
                Err(code) => return code,
            }
        }
        Command::New { .. } => match observe_or_exit(platform, request, observe::capture_new) {
            Ok(snapshot) => decide::new::decide(&snapshot, request),
            Err(code) => return code,
        },
        Command::Add(_) => match observe_or_exit(platform, request, observe::capture_edit) {
            Ok(snapshot) => decide::edit::add(&snapshot, request),
            Err(code) => return code,
        },
        Command::Delete { .. } => match observe_or_exit(platform, request, observe::capture_edit) {
            Ok(snapshot) => decide::edit::delete(&snapshot, request),
            Err(code) => return code,
        },
        Command::Set(_) => match observe_or_exit(platform, request, observe::capture_edit) {
            Ok(snapshot) => decide::edit::set(&snapshot, request),
            Err(code) => return code,
        },
        Command::Move(_) => match observe_or_exit(platform, request, observe::capture_edit) {
            Ok(snapshot) => decide::edit::move_partition(&snapshot, request),
            Err(code) => return code,
        },
        Command::Copy(_) => match observe_or_exit(platform, request, observe::capture_edit) {
            Ok(snapshot) => decide::edit::copy_partition(&snapshot, request),
            Err(code) => return code,
        },
        Command::Mkfs { .. } => {
            match observe_or_exit(platform, request, observe::capture_filesystem) {
                Ok(snapshot) => decide::mkfs::decide(&snapshot, request),
                Err(code) => return code,
            }
        }
        Command::Resizefs { .. } => {
            match observe_or_exit(platform, request, observe::capture_filesystem) {
                Ok(snapshot) => decide::resizefs::decide(&snapshot, request),
                Err(code) => return code,
            }
        }
        Command::Resize(_) => match observe_or_exit(platform, request, observe::capture_resize) {
            Ok(snapshot) => decide::resize::decide(&snapshot, request),
            Err(code) => return code,
        },
        Command::Recover(_) => match observe_or_exit(platform, request, observe::capture_recover) {
            Ok(snapshot) => decide::recover::decide(&snapshot, request),
            Err(code) => return code,
        },
    };
    conclude(platform, request, decision)
}

/// `records scan`：先证明根是可读目录，再交给判定层。
fn scan(platform: &dyn Platform, request: &observe::Request) -> u8 {
    let Some(path) = request.target() else {
        return usage();
    };
    if !platform.is_directory(path) {
        eprintln!(
            "refused: {} does not exist or is not a directory",
            path.display()
        );
        return exit::REFUSED;
    }
    let files = match platform.list_files(path) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("error: cannot scan {}: {error}", path.display());
            return exit::ENVIRONMENT;
        }
    };
    let records: Vec<(std::path::PathBuf, Vec<u8>)> = files
        .iter()
        .filter(|file| {
            file.file_name()
                .is_some_and(|name| name == observe::records::RECORD_FILE)
        })
        .filter_map(|file| std::fs::read(file).ok().map(|bytes| (file.clone(), bytes)))
        .collect();
    conclude(
        platform,
        request,
        decide::records::scan_with(path, &records),
    )
}

/// 采集失败直接折算成进程出口。
fn observe_or_exit<T>(
    platform: &dyn Platform,
    request: &observe::Request,
    capture: fn(&dyn Platform, &observe::Request) -> Result<T, observe::CaptureError>,
) -> Result<T, u8> {
    capture(platform, request).map_err(|error| capture_failed(request, platform, error))
}

/// 观察失败：只读命令把打不开的目标当作拒绝。
fn capture_failed(
    request: &observe::Request,
    platform: &dyn Platform,
    error: observe::CaptureError,
) -> u8 {
    let error = match error {
        observe::CaptureError::DeclaredIdentityNotApplicable => {
            eprintln!(
                "diskedit: --identity applies only to block devices with no readable WWN, EUI or \
                 repeatable serial; images and identified devices are addressed by their own identity"
            );
            return usage();
        }
        observe::CaptureError::SectorSizeOnBlockDevice => {
            eprintln!(
                "diskedit: --sector-size applies to image files only; a block device's logical \
                 sector size is reported by the kernel (BLKSSZGET) and cannot be overridden"
            );
            return exit::REFUSED;
        }
        observe::CaptureError::UnsupportedSector { bytes } => {
            eprintln!(
                "unsupported: the target's sector size {bytes} is outside the supported set \
                 (512..=65536, powers of two) — geometry cannot be constructed"
            );
            return exit::UNSUPPORTED;
        }
        // 宿主本身不提供这项能力：不是目标拒绝，而是平台不支持。
        observe::CaptureError::Platform(platform::PlatformError::NotSupported(why)) => {
            eprintln!("unsupported: {why}");
            return exit::UNSUPPORTED;
        }
        other @ (observe::CaptureError::Platform(_)
        | observe::CaptureError::NoTarget
        | observe::CaptureError::Identity(_)
        | observe::CaptureError::TooSmall { .. }
        | observe::CaptureError::Unreadable { .. }) => other,
    };
    let why = match &error {
        observe::CaptureError::Platform(error) => error.to_string(),
        observe::CaptureError::NoTarget => "the command needs a target".to_owned(),
        observe::CaptureError::Identity(error) => error.to_string(),
        observe::CaptureError::TooSmall { .. } => error.to_string(),
        observe::CaptureError::Unreadable { why } => why.clone(),
        observe::CaptureError::UnsupportedSector { .. } => unreachable!("上面已处理"),
        observe::CaptureError::DeclaredIdentityNotApplicable => unreachable!("上面已处理"),
        observe::CaptureError::SectorSizeOnBlockDevice => unreachable!("上面已处理"),
    };
    match request.command() {
        // `records list` 在目标读不到时仍然给出空列表，并以成功退出。
        Command::RecordsList => {
            eprintln!("note: {why}");
            conclude(
                platform,
                request,
                decide::records::list(None, platform.state_root()),
            )
        }
        // 装不下一个扇区的目标对 `check` 等于没有分区表；对 `info` 是读失败。
        Command::Check { .. } if matches!(error, observe::CaptureError::TooSmall { .. }) => {
            conclude(
                platform,
                request,
                Decision::Refuse(decide::Refusal::TableMissing),
            )
        }
        // `check` 已经进入执行阶段：目标字节读不出来是环境故障，不是拒绝。
        // 打开失败仍按拒绝处理，与基线一致。
        Command::Check { .. } if matches!(error, observe::CaptureError::Unreadable { .. }) => {
            eprintln!("error: {why}");
            exit::ENVIRONMENT
        }
        Command::Info { .. }
        | Command::Inspect
        | Command::RecordsVerify { .. }
        | Command::RecordsScan
        | Command::Resize(_)
        | Command::Move(_)
        | Command::Copy(_)
        | Command::Delete { .. }
        | Command::Set(_)
        | Command::Check { .. }
        | Command::Mkfs { .. }
        | Command::Resizefs { .. }
        | Command::Recover(_)
        | Command::New { .. }
        | Command::Add(_) => {
            eprintln!("refused: {why}");
            exit::REFUSED
        }
    }
}

/// 把判定结果落到进程出口。
fn conclude(platform: &dyn Platform, request: &observe::Request, decision: Decision) -> u8 {
    match decision {
        Decision::Refuse(refusal) => refuse(&refusal),
        Decision::Report { report, .. } => match report {
            decide::Report::Check(query) => check(platform, &query),
            other @ (decide::Report::Info(_)
            | decide::Report::Inspect(_)
            | decide::Report::Records(_)
            | decide::Report::Resize(_)) => match report::render(&other) {
                Some(report::Rendered::Json(text)) | Some(report::Rendered::Text(text)) => {
                    println!("{text}");
                    exit::SUCCESS
                }
                None => exit::INTERNAL,
            },
        },
        Decision::Execute { plan, note } => write(platform, request, plan, note),
        Decision::Unchanged { note } => {
            println!("{note}");
            exit::SUCCESS
        }
    }
}

/// 拒绝的统一出口。
fn refuse(refusal: &decide::Refusal) -> u8 {
    let code = exit::exit_of_refusal(refusal);
    let prefix = if code == exit::UNSUPPORTED {
        "unsupported"
    } else if code == exit::USAGE {
        "diskedit"
    } else {
        "refused"
    };
    eprintln!("{prefix}: {}", refusal.describe());
    eprintln!("— {}", refusal.next_step());
    code
}

/// 写命令：确认计划、执行并汇报终态。
fn write(
    platform: &dyn Platform,
    request: &observe::Request,
    plan: decide::Plan,
    note: String,
) -> u8 {
    let Some(path) = request.target() else {
        return usage();
    };
    // 在线形态的确认对象是承载挂载的整盘，而不是挂载点目录本身。
    let identity_path = plan
        .steps()
        .iter()
        .find_map(|step| match &step.kind {
            decide::StepKind::GrowFilesystemOnline { disk, .. } => Some(disk.as_path()),
            decide::StepKind::Inspect
            | decide::StepKind::Check { .. }
            | decide::StepKind::ListRecords
            | decide::StepKind::VerifyRecord { .. }
            | decide::StepKind::ScanRecords
            | decide::StepKind::WriteTable { .. }
            | decide::StepKind::AddPartition { .. }
            | decide::StepKind::DeletePartition { .. }
            | decide::StepKind::ResizePartition { .. }
            | decide::StepKind::SetProperty { .. }
            | decide::StepKind::MakeFilesystem { .. }
            | decide::StepKind::GrowFilesystem { .. }
            | decide::StepKind::ShrinkFilesystem { .. }
            | decide::StepKind::CopyPartition { .. }
            | decide::StepKind::LvmPvShrink { .. }
            | decide::StepKind::LvmGrowChain { .. }
            | decide::StepKind::MovePartition { .. }
            | decide::StepKind::RefreshKernelView
            | decide::StepKind::RecoverRecords { .. } => None,
        })
        .unwrap_or(path);
    let current = observe_identity(platform, identity_path);
    let plan = match decide::confirm(plan, request.confirmed(), &current) {
        Ok(plan) => plan,
        Err(refusal) => return refuse(&refusal),
    };
    let relocation = plan.steps().iter().any(|step| {
        matches!(
            step.kind,
            decide::StepKind::MovePartition { .. } | decide::StepKind::CopyPartition { .. }
        )
    });
    let recovery = plan
        .steps()
        .iter()
        .any(|step| matches!(step.kind, decide::StepKind::RecoverRecords { .. }));
    let filesystem = plan.steps().iter().any(|step| {
        matches!(
            step.kind,
            decide::StepKind::MakeFilesystem { .. }
                | decide::StepKind::GrowFilesystem { .. }
                | decide::StepKind::ShrinkFilesystem { .. }
        )
    });
    let table = plan
        .steps()
        .iter()
        .any(|step| matches!(step.kind, decide::StepKind::WriteTable { .. }));
    let online = plan
        .steps()
        .iter()
        .any(|step| matches!(step.kind, decide::StepKind::GrowFilesystemOnline { .. }));
    let outcome = if recovery {
        execute::recover::run(platform, path, &plan)
    } else if online {
        execute::filesystem::run_online(platform, &plan)
    } else if filesystem && table {
        execute::resize::run(platform, path, &plan)
    } else if filesystem {
        execute::filesystem::run(platform, path, &plan)
    } else if relocation {
        execute::relocate::run(platform, path, &plan)
    } else {
        execute::table::run(platform, path, &plan)
    };
    let code = exit::exit_of(&outcome);
    if outcome.succeeded() {
        println!("{note}");
    } else if let Some(reason) = outcome.reason() {
        if code == exit::PARTIAL {
            eprintln!("{}", report::text::recovery(reason));
        } else {
            let prefix = if code == exit::REFUSED {
                "refused"
            } else {
                "error"
            };
            eprintln!("{prefix}: {reason}");
        }
    }
    code
}

/// 锁外观测身份；执行器会在持锁后再次核对。
fn observe_identity(
    platform: &dyn Platform,
    path: &std::path::Path,
) -> diskedit_core::evidence::Observation<diskedit_core::fingerprint::Fingerprint> {
    use diskedit_core::evidence::{Generation, Observation, Source};
    use diskedit_core::fingerprint::Strength;
    match platform.identity(path) {
        Ok(fingerprint) if fingerprint.strength() == Strength::Strong => Observation::known(
            fingerprint,
            Source::new("platform.identity"),
            Generation::FIRST,
        ),
        Ok(_) => Observation::not_applicable("the target gives no comparable identity"),
        Err(error) => Observation::unknown(error.to_string()),
    }
}

/// 只读检查：执行层给出四类结论，这里映射退出码。
fn check(platform: &dyn Platform, query: &decide::report::CheckQuery) -> u8 {
    match execute::diagnose::run(platform, query) {
        execute::diagnose::DiagnoseResult::Clean { message } => {
            println!("{message}");
            exit::SUCCESS
        }
        execute::diagnose::DiagnoseResult::Problems { message } => {
            eprintln!("{message}");
            exit::REFUSED
        }
        execute::diagnose::DiagnoseResult::Missing { message } => {
            eprintln!("unsupported: {message}");
            exit::UNSUPPORTED
        }
        execute::diagnose::DiagnoseResult::Refused { message } => {
            eprintln!("refused: {message}");
            eprintln!("— this run changed no persistent state");
            exit::REFUSED
        }
        execute::diagnose::DiagnoseResult::Environment { message } => {
            eprintln!("error: {message}");
            exit::ENVIRONMENT
        }
    }
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
