//! 分派边界：写命令在碰目标之前被拒绝，写命令必须带 `--yes`。
//!
//! 这两个行为都取决于命令面声明的 effect；用例直接跑命令行，不重复声明
//! "哪些命令会写"。

use std::path::{Path, PathBuf};

use diskedit::app::cli::dispatch_with;
use diskedit::app::report::exit;
use diskedit::platform::{
    LoopBinding, Occupancy, Platform, PlatformError, PlatformResult, RecordEntry, StateLock,
    TargetHandle, TargetKind, ToolRun,
};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;

/// 每条写命令的最短命令行（含 `--yes`）。
const WRITE_COMMANDS: &[&[&str]] = &[
    &["undo", "disk.img", "--yes"],
    &["replay", "disk.img", "--yes"],
    &["release", "disk.img", "--yes"],
    &["resize", "disk.img:1", "--end", "4096", "--yes"],
    &["move", "disk.img:1", "--start", "2048", "--yes"],
    &["copy", "disk.img:1", "--start", "2048", "--yes"],
    &["delete", "disk.img:1", "--yes"],
    &["set", "disk.img:1", "name", "data", "--yes"],
    &["mkfs", "disk.img:1", "ext4", "--yes"],
    &["new", "disk.img", "--yes"],
    &["add", "disk.img", "1M", "--yes"],
];

/// 不能写目标、其余端口一律未接入的宿主。
///
/// 进入任一端口都计数：写命令若真的走到端口，计数就不再为零。
#[derive(Default)]
struct ReadOnlyHost {
    calls: std::sync::atomic::AtomicUsize,
}

impl ReadOnlyHost {
    fn note(&self) {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn unwired() -> PlatformError {
    PlatformError::not_supported("the backend is not wired in this test host")
}

impl Platform for ReadOnlyHost {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        self.note();
        Err(unwired())
    }

    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        self.note();
        Err(unwired())
    }

    fn path_kind(&self, _path: &Path) -> TargetKind {
        self.note();
        TargetKind::Other
    }

    fn is_partition_node(&self, _path: &Path) -> bool {
        false
    }

    fn identity(&self, _path: &Path) -> PlatformResult<Fingerprint> {
        self.note();
        Err(unwired())
    }

    fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
        Err(unwired())
    }

    fn state_root(&self) -> Option<PathBuf> {
        None
    }

    fn is_directory(&self, _path: &Path) -> bool {
        false
    }

    fn list_record_entries(&self, _root: &Path, _name: &str) -> PlatformResult<Vec<PathBuf>> {
        Err(unwired())
    }

    fn read_record(&self, _path: &Path) -> PlatformResult<RecordEntry> {
        Err(unwired())
    }

    fn write_record(&self, _path: &Path, _bytes: &[u8]) -> PlatformResult<()> {
        Err(unwired())
    }

    fn remove_record(&self, _path: &Path) -> PlatformResult<()> {
        Err(unwired())
    }

    fn rename_record(&self, _from: &Path, _to: &Path) -> PlatformResult<()> {
        Err(unwired())
    }

    fn run(&self, _program: Program, _args: &[String]) -> PlatformResult<ToolRun> {
        Err(unwired())
    }

    fn tool_available(&self, _program: Program) -> bool {
        false
    }

    fn tool_name(&self, _program: Program) -> &'static str {
        "unavailable"
    }

    fn bind_loop(&self, _path: &Path, _offset: u64, _len: u64) -> PlatformResult<LoopBinding> {
        Err(unwired())
    }

    fn partition_device(&self, _disk: &Path, _number: u32) -> PlatformResult<PathBuf> {
        Err(unwired())
    }

    fn entropy(&self, _len: usize) -> PlatformResult<Vec<u8>> {
        Err(unwired())
    }

    fn create_directory_all(&self, _path: &Path) -> PlatformResult<()> {
        Err(unwired())
    }

    fn remove_directory(&self, _path: &Path) -> PlatformResult<()> {
        Err(unwired())
    }

    fn temporary_directory(&self, _tag: &str) -> PlatformResult<PathBuf> {
        Err(unwired())
    }

    fn lock_state(&self, _path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        Err(unwired())
    }

    fn supports_writes(&self) -> bool {
        false
    }
}

/// 以给定宿主跑一条命令，返回退出码与进入端口的次数。
fn dispatch_on(host: &ReadOnlyHost, argv: &[&str]) -> (u8, usize) {
    let before = host.calls();
    let args: Vec<std::ffi::OsString> = std::iter::once("diskedit")
        .chain(argv.iter().copied())
        .map(std::ffi::OsString::from)
        .collect();
    let code = dispatch_with(host, args);
    (code, host.calls() - before)
}

/// 命令行对应的入口名：`records` 的子命令用空格拼接，其余取第一个词。
fn entry_name(argv: &[&str]) -> String {
    match argv {
        ["records", subcommand, ..] => format!("records {subcommand}"),
        [first, ..] => (*first).to_owned(),
        [] => String::new(),
    }
}

#[test]
fn a_write_is_refused_before_the_target_is_opened() {
    for argv in WRITE_COMMANDS {
        let host = ReadOnlyHost::default();
        let (code, calls) = dispatch_on(&host, argv);
        assert_eq!(
            code,
            exit::UNSUPPORTED,
            "`{}` 在不能写的宿主上应按能力拒绝",
            argv.join(" ")
        );
        assert_eq!(calls, 0, "`{}` 必须在打开目标之前被拒绝", argv.join(" "));
    }
}

#[test]
fn the_write_command_list_matches_the_declared_surface() {
    let declared: Vec<&str> = diskedit::app::cli::declared_entries()
        .filter(|name| diskedit::app::cli::declare::is_write_entry(name))
        .collect();
    let enumerated: Vec<String> = WRITE_COMMANDS.iter().map(|argv| entry_name(argv)).collect();
    assert_eq!(declared, enumerated, "写命令用例与命令面声明不一致");
}

#[test]
fn a_read_only_command_reaches_the_port() {
    // 只读命令不受能力门约束：它确实走进端口，并被"未接入"拒绝。
    let host = ReadOnlyHost::default();
    let (code, calls) = dispatch_on(&host, &["layout", "disk.img"]);
    assert_eq!(code, exit::UNSUPPORTED);
    assert!(calls > 0, "只读命令应当进入端口");
}

#[test]
fn a_dry_run_is_not_a_write() {
    // `--dry-run` 只预览，不写盘；能力门放行，命令照常进入端口。
    let host = ReadOnlyHost::default();
    let (code, calls) = dispatch_on(&host, &["add", "disk.img", "1M", "--dry-run"]);
    assert_eq!(code, exit::UNSUPPORTED);
    assert!(calls > 0, "--dry-run 不是写，能力门不应拦下它");
}

#[test]
fn every_write_command_demands_confirmation() {
    for argv in WRITE_COMMANDS {
        let without_yes: Vec<&str> = argv
            .iter()
            .copied()
            .filter(|argument| *argument != "--yes")
            .collect();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
            .args(&without_yes)
            .output()
            .unwrap_or_else(|error| panic!("运行 {without_yes:?} 失败: {error}"));
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{}` 缺 --yes 应是用法错误",
            without_yes.join(" ")
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("needs --yes"),
            "`{}` 的提示应点名缺 --yes：{stderr}",
            without_yes.join(" ")
        );
    }
}
