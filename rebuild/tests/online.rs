//! 在线扩容：挂载点形态的执行路径、工具结局三态与回读先后。
//!
//! 这些用例用内存宿主驱动真实执行器，不挂载任何设备；真实挂载与来宾里的扩容由
//! `tests/carrier.rs` 在具备 QEMU 与 bundle 时执行。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use diskedit::app::decide::{Plan, Step, StepKind, confirm};
use diskedit::app::execute::{ExecutionState, filesystem};
use diskedit::app::report::exit;
use diskedit::platform::{
    Carrier, LoopBinding, MountFacts, Occupancy, OnlineReport, OnlineRequest, Platform,
    PlatformError, PlatformResult, StateLock, TargetHandle, TargetKind, ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{FsKind, Program};

const IDENTITY: Fingerprint = Fingerprint::strong([0x11; 32]);

struct Lock;
impl StateLock for Lock {}

/// 一次外部工具调用：启动并以给定退出码结束。
#[derive(Clone)]
enum Call {
    Exits(Option<i32>, String),
}

struct Fake {
    calls: Mutex<Vec<Program>>,
    replies: Mutex<std::collections::VecDeque<Call>>,
    carrier: Mutex<Option<OnlineReport>>,
    carrier_calls: Mutex<usize>,
    native: bool,
    kind: TargetKind,
}

impl Fake {
    fn new(replies: Vec<Call>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
            carrier: Mutex::new(None),
            carrier_calls: Mutex::new(0),
            native: true,
            kind: TargetKind::Other,
        }
    }

    fn with_carrier(report: OnlineReport) -> Self {
        let mut fake = Self::new(Vec::new());
        *fake.carrier.lock().unwrap() = Some(report);
        fake.native = false;
        fake
    }

    fn calls(&self) -> Vec<Program> {
        self.calls.lock().unwrap().clone()
    }

    fn carrier_calls(&self) -> usize {
        *self.carrier_calls.lock().unwrap()
    }
}

impl Platform for Fake {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Err(PlatformError::not_supported("no target in this test"))
    }

    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Err(PlatformError::not_supported("no target in this test"))
    }

    fn path_kind(&self, _path: &Path) -> TargetKind {
        self.kind
    }

    fn is_partition_node(&self, _path: &Path) -> bool {
        false
    }

    fn identity(&self, _path: &Path) -> PlatformResult<Fingerprint> {
        Ok(IDENTITY)
    }

    fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
        Ok(Occupancy::Idle)
    }

    fn state_root(&self) -> Option<PathBuf> {
        None
    }

    fn is_directory(&self, _path: &Path) -> bool {
        false
    }

    fn list_files(&self, _root: &Path) -> PlatformResult<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    fn read_file(&self, _path: &Path) -> PlatformResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn run(&self, program: Program, _args: &[String]) -> PlatformResult<ToolRun> {
        self.calls.lock().unwrap().push(program);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Call::Exits(Some(0), String::new()));
        match reply {
            Call::Exits(code, stdout) => Ok(ToolRun {
                code,
                stdout,
                stderr: String::new(),
            }),
        }
    }

    fn tool_available(&self, _program: Program) -> bool {
        self.native
    }

    fn tool_name(&self, _program: Program) -> &'static str {
        "tool"
    }

    fn bind_loop(&self, _path: &Path, _offset: u64, _len: u64) -> PlatformResult<LoopBinding> {
        Err(PlatformError::not_supported("no loop device in this test"))
    }

    fn partition_device(&self, _disk: &Path, _number: u32) -> PlatformResult<PathBuf> {
        Err(PlatformError::not_supported("no partitions in this test"))
    }

    fn entropy(&self, len: usize) -> PlatformResult<Vec<u8>> {
        Ok(vec![0u8; len])
    }

    fn create_directory_all(&self, _path: &Path) -> PlatformResult<()> {
        Ok(())
    }

    fn remove_directory(&self, _path: &Path) -> PlatformResult<()> {
        Ok(())
    }

    fn temporary_directory(&self, _tag: &str) -> PlatformResult<PathBuf> {
        Ok(PathBuf::from("/tmp"))
    }

    fn write_state_file(&self, _path: &Path, _bytes: &[u8]) -> PlatformResult<()> {
        Ok(())
    }

    fn append_state_file(&self, _path: &Path, _bytes: &[u8]) -> PlatformResult<()> {
        Ok(())
    }

    fn remove_state_file(&self, _path: &Path) -> PlatformResult<()> {
        Ok(())
    }

    fn sync_directory(&self, _path: &Path) -> PlatformResult<()> {
        Ok(())
    }

    fn lock_state(&self, _path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        Ok(Box::new(Lock))
    }

    fn mount_facts(&self, _path: &Path) -> PlatformResult<MountFacts> {
        Ok(MountFacts::Mounted {
            mount_point: PathBuf::from("/mnt"),
            device: PathBuf::from("/dev/sda1"),
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
        })
    }

    fn carrier(&self) -> PlatformResult<Option<Box<dyn Carrier>>> {
        let Some(report) = self.carrier.lock().unwrap().clone() else {
            return Ok(None);
        };
        *self.carrier_calls.lock().unwrap() += 1;
        Ok(Some(Box::new(ReportCarrier { report })))
    }
}

struct ReportCarrier {
    report: OnlineReport,
}

impl Carrier for ReportCarrier {
    fn online_grow(&self, _request: &OnlineRequest) -> PlatformResult<OnlineReport> {
        Ok(self.report.clone())
    }
}

fn plan() -> diskedit::app::decide::ConfirmedPlan {
    plan_for(FsKind::Ext)
}

fn plan_for(filesystem: FsKind) -> diskedit::app::decide::ConfirmedPlan {
    let step = Step::new(
        StepKind::GrowFilesystemOnline {
            mount_point: PathBuf::from("/mnt"),
            device: PathBuf::from("/dev/sda1"),
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
            filesystem,
            bytes: 64 << 20,
        },
        Vec::new(),
    );
    let plan = Plan::external(vec![step], IDENTITY).expect("在线计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

fn geometry(blocks: u64, size: u64) -> String {
    format!("Block count: {blocks}\nBlock size: {size}\n")
}

// covers: B28, I12
#[test]
fn online_grow_succeeds_only_after_the_read_back_reaches_the_partition() {
    let fake = Fake::new(vec![
        Call::Exits(Some(0), String::new()),
        Call::Exits(Some(0), geometry(65536, 1024)),
    ]);
    let outcome = filesystem::run_online(&fake, &plan());
    assert_eq!(outcome.state(), ExecutionState::Committed);
    assert_eq!(exit::exit_of(&outcome), exit::SUCCESS);
    assert_eq!(fake.calls(), vec![Program::Resize2fs, Program::Dumpe2fs]);
}

// covers: I12
#[test]
fn a_tool_without_an_exit_status_is_indeterminate_and_needs_recovery() {
    let fake = Fake::new(vec![Call::Exits(None, String::new())]);
    let outcome = filesystem::run_online(&fake, &plan());
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
    assert_eq!(fake.calls(), vec![Program::Resize2fs]);
}

// covers: I12
#[test]
fn a_failed_read_back_is_partial_even_when_the_tool_exited_zero() {
    let fake = Fake::new(vec![
        Call::Exits(Some(0), String::new()),
        Call::Exits(Some(1), String::new()),
    ]);
    let outcome = filesystem::run_online(&fake, &plan());
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
    assert_eq!(fake.calls(), vec![Program::Resize2fs, Program::Dumpe2fs]);
}

// covers: B28, I12
#[test]
fn a_short_filesystem_after_the_tool_is_partial() {
    let fake = Fake::new(vec![
        Call::Exits(Some(0), String::new()),
        Call::Exits(Some(0), geometry(16384, 1024)),
    ]);
    let outcome = filesystem::run_online(&fake, &plan());
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
}

// covers: R0792
#[test]
fn a_sector_size_on_a_block_device_is_refused_before_it_is_opened() {
    let mut fake = Fake::new(Vec::new());
    fake.kind = TargetKind::BlockDevice;
    let code = diskedit::app::cli::dispatch_with(
        &fake,
        ["diskedit", "info", "/dev/sda", "--sector-size", "4096"].map(std::ffi::OsString::from),
    );
    assert_eq!(code, exit::REFUSED);
    assert!(fake.calls().is_empty(), "rejection precedes any tool");
}

// covers: B28
#[test]
fn a_host_without_the_tool_hands_the_grow_to_the_carrier() {
    let fake = Fake::with_carrier(OnlineReport {
        blocks: Some(65536),
        block_size: Some(1024),
    });
    let outcome = filesystem::run_online(&fake, &plan());
    assert_eq!(outcome.state(), ExecutionState::Committed);
    assert_eq!(fake.carrier_calls(), 1);
}

/// btrfs 的超级块探针：`total_bytes` 落在最后一个分配单元内即达标，输出里
/// `dev_item.total_bytes` 之类的同名字段不得被当成本卷尺寸。
fn superblock(total: u64, sectorsize: u64) -> String {
    format!(
        "superblock: bytenr=65536, device=/dev/sda1\n\
         sectorsize              {sectorsize}\n\
         total_bytes             {total}\n\
         dev_item.total_bytes    1099511627776\n"
    )
}

// covers: B28, I12
#[test]
fn an_online_btrfs_grow_is_verified_from_the_superblock() {
    let fake = Fake::new(vec![
        Call::Exits(Some(0), String::new()),
        Call::Exits(Some(0), superblock(64 << 20, 4096)),
    ]);
    let outcome = filesystem::run_online(&fake, &plan_for(FsKind::Btrfs));
    assert_eq!(outcome.state(), ExecutionState::Committed);
    assert_eq!(exit::exit_of(&outcome), exit::SUCCESS);
    assert_eq!(fake.calls(), vec![Program::Btrfs, Program::Btrfs]);
}

// covers: B28, I12
#[test]
fn an_online_btrfs_grow_that_stopped_short_is_partial() {
    let fake = Fake::new(vec![
        Call::Exits(Some(0), String::new()),
        Call::Exits(Some(0), superblock(32 << 20, 4096)),
    ]);
    let outcome = filesystem::run_online(&fake, &plan_for(FsKind::Btrfs));
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
}
