//! 在线扩容：挂载点形态的执行路径、工具结局三态与回读先后。
//!
//! 这些用例用内存宿主驱动真实执行器，不挂载任何设备；真实挂载与来宾里的扩容由
//! `tests/carrier.rs` 在具备 QEMU 与 bundle 时执行。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diskedit::app::decide::step::TableSegment;
use diskedit::app::decide::{Plan, Report, Step, StepKind, confirm};
use diskedit::app::execute::{ExecutionState, filesystem, resize};
use diskedit::app::observe::records::RecordState;
use diskedit::app::observe::resize::{LvmFacts, OnlineFacts, ResizeSnapshot};
use diskedit::app::observe::table::{MbrView, PartitionView, TableView};
use diskedit::app::observe::{
    Command, PartitionSelector, PriorState, Request, ResizeRequest, SizeSpec, TableFacts,
    TableFormat, TargetFacts, TargetSpec,
};
use diskedit::app::report::exit;
use diskedit::platform::{
    Carrier, LoopBinding, MountFacts, Occupancy, OnlineReport, OnlineRequest, PartitionMount,
    Platform, PlatformError, PlatformResult, RecordEntry, StateLock, TargetHandle, TargetKind,
    ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{ContentKind, FsKind, Program};
use diskedit_core::region::Region;
use diskedit_core::table::stamp::{stamp_regions, table_stamp};

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

    fn list_record_entries(&self, _root: &Path, _name: &str) -> PlatformResult<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    fn read_record(&self, _path: &Path) -> PlatformResult<RecordEntry> {
        Ok(RecordEntry::Absent)
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

    fn write_record(&self, _path: &Path, _bytes: &[u8]) -> PlatformResult<()> {
        Ok(())
    }

    fn remove_record(&self, _path: &Path) -> PlatformResult<()> {
        Ok(())
    }

    fn rename_record(&self, _from: &Path, _to: &Path) -> PlatformResult<()> {
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
        ["diskedit", "layout", "/dev/sda", "--sector-size", "4096"].map(std::ffi::OsString::from),
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

// --- 已挂载分区的几何调整（在线 `resize`） ---------------------------------

const DISK_BYTES: u64 = 8192 * 512;

/// 一条 MBR 主分区记录；`start`/`sectors` 都是 LBA。
fn mbr_lba0(start: u32, sectors: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 512];
    bytes[446] = 0x00;
    bytes[447..450].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    bytes[450] = 0x83;
    bytes[451..454].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    bytes[454..458].copy_from_slice(&start.to_le_bytes());
    bytes[458..462].copy_from_slice(&sectors.to_le_bytes());
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    bytes
}

fn partition_view(start: u64, sectors: u64) -> PartitionView {
    PartitionView {
        number: 1,
        first_lba: start,
        last_lba: start + sectors - 1,
        type_text: "0x83".to_owned(),
        name: String::new(),
        base_bytes: start * 512,
        bytes: sectors * 512,
        content: ContentKind::Filesystem(FsKind::Ext),
    }
}

fn absent_record() -> RecordState {
    RecordState {
        directory: None,
        directory_exists: false,
        file: None,
        bytes: None,
        unavailable: None,
    }
}

/// 已挂载分区的 MBR 表现场：分区 1 从 `start` 起占 `sectors` 个扇区。
fn online_snapshot(start: u64, sectors: u64) -> ResizeSnapshot {
    let partition = partition_view(start, sectors);
    ResizeSnapshot {
        target: TargetFacts {
            display: "/dev/sda".to_owned(),
            path: PathBuf::from("/dev/sda"),
            bytes: DISK_BYTES,
            kind: TargetKind::File,
            sector_size: 512,
            identity: Some(IDENTITY),
        },
        occupancy: Occupancy::Idle,
        record: absent_record(),
        prior: PriorState::Absent,
        view: TableView::Mbr(MbrView {
            sector_size: 512,
            partitions: vec![partition.clone()],
        }),
        facts: Some(TableFacts::Mbr {
            lba0: mbr_lba0(
                u32::try_from(start).expect("起点可表示"),
                u32::try_from(sectors).expect("长度可表示"),
            ),
            total_lba: DISK_BYTES / 512,
        }),
        whole_content: None,
        stamp: [0u8; 32],
        is_partition_node: false,
        selector: Some(PartitionSelector::Number(1)),
        selected: Some(partition),
        tools: vec![
            (Program::E2fsck, true),
            (Program::Resize2fs, true),
            (Program::Dumpe2fs, true),
        ],
        lvm: None,
        online: Some(OnlineFacts {
            mount_point: PathBuf::from("/mnt"),
            device: PathBuf::from("/dev/sda1"),
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
        }),
        partition_mount: None,
        swap_active: Some(false),
        min_bytes: None,
    }
}

fn resize_command(size: Option<SizeSpec>, end: Option<u64>) -> ResizeRequest {
    ResizeRequest {
        partition: None,
        size,
        end,
        grow_to_end: false,
        align: None,
        allow_move: false,
        chunk: None,
        grow_lv: false,
        lv: None,
        no_fs: false,
        fs_only: false,
        dry_run: false,
    }
}

fn request_for(resize: ResizeRequest) -> Request {
    Request::new(TargetSpec::new("/mnt", None), Command::Resize(resize)).with_confirmation(true)
}

fn resize_request(size: Option<SizeSpec>, end: Option<u64>) -> Request {
    request_for(resize_command(size, end))
}

// covers: R0421, R0422, R0423, R0424, R0425, R0426, R0427, R0428, R0429, R0430, R0431, R0432,
// covers: R0433, R0434
#[test]
fn online_resize_plans_table_then_kernel_view_then_filesystem() {
    let snapshot = online_snapshot(2048, 1024);
    let request = resize_request(Some(SizeSpec::Grow), None);
    let decision = diskedit::app::decide::resize::decide(&snapshot, &request);
    let diskedit::app::decide::Decision::Execute { plan, .. } = decision else {
        panic!("an online grow must plan executable steps: {decision:?}");
    };
    let kinds: Vec<&StepKind> = plan.steps().iter().map(|step| &step.kind).collect();
    assert!(matches!(kinds[0], StepKind::WriteTable { .. }), "{kinds:?}");
    let StepKind::RefreshKernelView {
        disk,
        partition,
        start_bytes,
        bytes,
    } = kinds[1]
    else {
        panic!("the second step must sync the kernel view: {kinds:?}");
    };
    assert_eq!(disk, &PathBuf::from("/dev/sda"));
    assert_eq!(*partition, 1);
    assert_eq!(*start_bytes, 2048 * 512);
    assert_eq!(*bytes, 6144 * 512);
    let StepKind::GrowFilesystemOnline {
        mount_point,
        disk,
        partition,
        bytes,
        ..
    } = kinds[2]
    else {
        panic!("the third step must grow the filesystem: {kinds:?}");
    };
    assert_eq!(mount_point, &PathBuf::from("/mnt"));
    assert_eq!(disk, &PathBuf::from("/dev/sda"));
    assert_eq!(*partition, 1);
    assert_eq!(*bytes, 6144 * 512);
}

// covers: R0428, R0429, R0430
#[test]
fn online_resize_refuses_flags_the_online_path_cannot_honour() {
    let cases: [fn(&mut ResizeRequest); 3] = [
        |resize| resize.no_fs = true,
        |resize| resize.allow_move = true,
        |resize| resize.chunk = Some(1 << 20),
    ];
    for mutate in cases {
        let snapshot = online_snapshot(2048, 1024);
        let mut resize = resize_command(Some(SizeSpec::Grow), None);
        mutate(&mut resize);
        let request = request_for(resize);
        let diskedit::app::decide::Decision::Refuse(refusal) =
            diskedit::app::decide::resize::decide(&snapshot, &request)
        else {
            panic!("the online path must refuse this flag: {request:?}");
        };
        assert_eq!(exit::exit_of_refusal(&refusal), exit::REFUSED);
    }
}

// covers: R0742, R0743
#[test]
fn online_resize_refuses_shrinking_a_mounted_partition_with_a_non_btrfs_filesystem() {
    let snapshot = online_snapshot(2048, 1024);
    let request = resize_request(None, Some(3000));
    let diskedit::app::decide::Decision::Refuse(refusal) =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("only btrfs can shrink while mounted");
    };
    assert_eq!(exit::exit_of_refusal(&refusal), exit::REFUSED);
}

/// 已挂载的 btrfs：文件系统族与在线工具都具备。
fn btrfs_snapshot(start: u64, sectors: u64) -> ResizeSnapshot {
    let mut snapshot = online_snapshot(start, sectors);
    let content = ContentKind::Filesystem(FsKind::Btrfs);
    if let Some(partition) = snapshot.selected.as_mut() {
        partition.content = content;
    }
    if let TableView::Mbr(view) = &mut snapshot.view {
        view.partitions[0].content = content;
    }
    snapshot.tools.push((Program::Btrfs, true));
    snapshot
}

/// 离线 ext 现场：无挂载、无在线信息，分区 1 为 ext。
fn offline_ext_snapshot(start: u64, sectors: u64) -> ResizeSnapshot {
    let mut snapshot = online_snapshot(start, sectors);
    snapshot.online = None;
    snapshot.partition_mount = None;
    snapshot.min_bytes = None;
    snapshot
}

// covers: R0211, R0382, R0416
#[test]
fn offline_ext_shrink_below_the_probed_minimum_is_refused_before_any_write() {
    let mut snapshot = offline_ext_snapshot(2048, 1024);
    snapshot.min_bytes = Some(Ok(600_000));
    // 收缩到分区 1 的 2500 LBA：新尺寸 453 扇区 = 231936 字节 < 探针最小值。
    let decision =
        diskedit::app::decide::resize::decide(&snapshot, &resize_request(None, Some(2500)));
    let diskedit::app::decide::Decision::Refuse(refusal) = decision else {
        panic!("收缩低于 resize2fs -P 的最小尺寸必须拒绝：{decision:?}");
    };
    assert!(
        matches!(
            refusal,
            diskedit::app::decide::Refusal::BelowMinimumSize {
                minimum: 600_000,
                ..
            }
        ),
        "{refusal:?}"
    );
}

// covers: R0211, R0382, R0416
#[test]
fn offline_ext_shrink_with_an_unreadable_minimum_is_refused_as_unknown() {
    let mut snapshot = offline_ext_snapshot(2048, 1024);
    snapshot.min_bytes = Some(Err("resize2fs -P exited Some(1): bad".to_owned()));
    let decision =
        diskedit::app::decide::resize::decide(&snapshot, &resize_request(None, Some(2500)));
    let diskedit::app::decide::Decision::Refuse(refusal) = decision else {
        panic!("探针失败必须按最小值未知拒绝：{decision:?}");
    };
    assert!(
        matches!(
            refusal,
            diskedit::app::decide::Refusal::MinimumSizeUnknown { .. }
        ),
        "{refusal:?}"
    );
}

// covers: R0211, R0382, R0416
#[test]
fn offline_ext_shrink_above_the_probed_minimum_plans_the_shrink() {
    let mut snapshot = offline_ext_snapshot(2048, 1024);
    snapshot.min_bytes = Some(Ok(1024));
    let decision =
        diskedit::app::decide::resize::decide(&snapshot, &resize_request(None, Some(2500)));
    let diskedit::app::decide::Decision::Execute { plan, .. } = decision else {
        panic!("高于最小值应可收缩：{decision:?}");
    };
    assert!(
        plan.steps()
            .iter()
            .any(|step| matches!(step.kind, StepKind::ShrinkFilesystem { .. })),
        "{:?}",
        plan.steps()
            .iter()
            .map(|step| &step.kind)
            .collect::<Vec<_>>()
    );
}

// covers: B28, R0742
#[test]
fn online_btrfs_shrink_plans_filesystem_then_table_then_kernel_view() {
    let snapshot = btrfs_snapshot(2048, 1024);
    let request = resize_request(None, Some(2500));
    let diskedit::app::decide::Decision::Execute { plan, .. } =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("a mounted btrfs can shrink online");
    };
    let kinds: Vec<&StepKind> = plan.steps().iter().map(|step| &step.kind).collect();
    let StepKind::ShrinkFilesystemOnline {
        mount_point,
        device,
        disk,
        partition,
        filesystem,
        bytes,
    } = kinds[0]
    else {
        panic!("the shrink must happen before the table changes: {kinds:?}");
    };
    assert_eq!(mount_point, &PathBuf::from("/mnt"));
    assert_eq!(device, &PathBuf::from("/dev/sda1"));
    assert_eq!(disk, &PathBuf::from("/dev/sda"));
    assert_eq!(*partition, 1);
    assert_eq!(*filesystem, FsKind::Btrfs);
    assert_eq!(*bytes, 453 * 512);
    assert!(matches!(kinds[1], StepKind::WriteTable { .. }), "{kinds:?}");
    let StepKind::RefreshKernelView { bytes: synced, .. } = kinds[2] else {
        panic!("the kernel view must follow the table: {kinds:?}");
    };
    assert_eq!(*synced, 453 * 512);
}

// covers: B28, R0742
#[test]
fn online_btrfs_shrink_without_the_tool_is_refused_before_any_write() {
    let mut snapshot = btrfs_snapshot(2048, 1024);
    snapshot.tools = vec![(Program::Btrfs, false)];
    let request = resize_request(None, Some(2500));
    let diskedit::app::decide::Decision::Refuse(refusal) =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("a missing btrfs tool must refuse the shrink");
    };
    assert_eq!(exit::exit_of_refusal(&refusal), exit::UNSUPPORTED);
}

// covers: B28, R0742
#[test]
fn an_online_btrfs_shrink_preview_reports_the_shrink_direction() {
    let snapshot = btrfs_snapshot(2048, 1024);
    let mut resize = resize_command(None, Some(2500));
    resize.dry_run = true;
    let request = request_for(resize);
    let diskedit::app::decide::Decision::Report { report, .. } =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("a dry run must only report");
    };
    let Report::Resize(preview) = report else {
        panic!("a resize dry run reports a resize preview");
    };
    assert!(!preview.grow);
    assert_eq!(preview.new_end_lba, 2500);
    assert_eq!(
        preview.filesystem,
        Some(ContentKind::Filesystem(FsKind::Btrfs))
    );
}

// covers: R0432
#[test]
fn online_grow_without_free_space_is_refused() {
    let snapshot = online_snapshot(2048, 6144);
    let request = resize_request(Some(SizeSpec::Grow), None);
    let diskedit::app::decide::Decision::Refuse(refusal) =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("grow with no free space must be refused");
    };
    assert_eq!(exit::exit_of_refusal(&refusal), exit::REFUSED);
}

struct MemHandle {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TargetHandle for MemHandle {
    fn path(&self) -> &Path {
        Path::new("memory")
    }
    fn len(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn kind(&self) -> TargetKind {
        TargetKind::File
    }
    fn sector_size(&self) -> Option<u64> {
        Some(512)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        let bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(bytes.len());
        let len = end - start;
        buf[..len].copy_from_slice(&bytes[start..end]);
        Ok(len)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        let mut bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > bytes.len() {
            return Err(PlatformError::parse("write past the memory image"));
        }
        bytes[start..end].copy_from_slice(buf);
        Ok(buf.len())
    }
    fn barrier(&mut self) -> PlatformResult<()> {
        Ok(())
    }
    fn refresh_partitions(&mut self) -> PlatformResult<()> {
        Ok(())
    }
}

/// 内存宿主：带上表字节、记录目录与内核视图调用记录。
struct LivePlatform {
    image: Mutex<Vec<u8>>,
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    live: Mutex<Vec<(u32, u64, u64)>>,
    live_result: Mutex<Result<(), String>>,
    replies: Mutex<std::collections::VecDeque<Call>>,
    calls: Mutex<Vec<Program>>,
    arg_calls: Mutex<Vec<(Program, Vec<String>)>>,
    kind: TargetKind,
    ready: bool,
}

impl LivePlatform {
    fn new(bytes: Vec<u8>, replies: Vec<Call>) -> Self {
        Self {
            image: Mutex::new(bytes),
            files: Mutex::new(BTreeMap::new()),
            live: Mutex::new(Vec::new()),
            live_result: Mutex::new(Ok(())),
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
            arg_calls: Mutex::new(Vec::new()),
            kind: TargetKind::File,
            ready: false,
        }
    }

    /// 块设备目标：具备块层能力，`partition_device` 给出分区节点。
    fn new_block(bytes: Vec<u8>, replies: Vec<Call>) -> Self {
        let mut platform = Self::new(bytes, replies);
        platform.kind = TargetKind::BlockDevice;
        platform.ready = true;
        platform
    }

    fn calls(&self) -> Vec<Program> {
        self.calls.lock().unwrap().clone()
    }

    fn arg_calls(&self) -> Vec<(Program, Vec<String>)> {
        self.arg_calls.lock().unwrap().clone()
    }

    fn live_calls(&self) -> Vec<(u32, u64, u64)> {
        self.live.lock().unwrap().clone()
    }
}

impl Platform for LivePlatform {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(MemHandle {
            bytes: Arc::new(Mutex::new(self.image.lock().unwrap().clone())),
        }))
    }
    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        self.open_target(_path)
    }
    fn path_kind(&self, _path: &Path) -> TargetKind {
        self.kind
    }
    fn filesystem_ready(&self, _path: &Path) -> bool {
        self.ready
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
        Some(PathBuf::from("/state"))
    }
    fn is_directory(&self, path: &Path) -> bool {
        path == Path::new("/mnt")
    }
    fn list_record_entries(&self, root: &Path, name: &str) -> PlatformResult<Vec<PathBuf>> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .keys()
            .filter(|path| {
                path.starts_with(root) && path.file_name().is_some_and(|entry| entry == name)
            })
            .cloned()
            .collect())
    }
    fn read_record(&self, path: &Path) -> PlatformResult<RecordEntry> {
        Ok(match self.files.lock().unwrap().get(path).cloned() {
            Some(bytes) => RecordEntry::Contents(bytes),
            None => RecordEntry::Absent,
        })
    }
    fn run(&self, program: Program, _args: &[String]) -> PlatformResult<ToolRun> {
        self.calls.lock().unwrap().push(program);
        self.arg_calls
            .lock()
            .unwrap()
            .push((program, _args.to_vec()));
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
        true
    }
    fn tool_name(&self, _program: Program) -> &'static str {
        "tool"
    }
    fn bind_loop(&self, _path: &Path, _offset: u64, _len: u64) -> PlatformResult<LoopBinding> {
        Err(PlatformError::not_supported("no loop device in this test"))
    }
    fn partition_device(&self, _disk: &Path, _number: u32) -> PlatformResult<PathBuf> {
        Ok(PathBuf::from("/dev/sda1"))
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
    fn write_record(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }
    fn remove_record(&self, path: &Path) -> PlatformResult<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }
    fn rename_record(&self, from: &Path, to: &Path) -> PlatformResult<()> {
        let mut files = self.files.lock().unwrap();
        if let Some(bytes) = files.remove(from) {
            files.insert(to.to_path_buf(), bytes);
        }
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
    fn partition_mount(&self, _disk: &Path, _partition: u32) -> PlatformResult<PartitionMount> {
        Ok(PartitionMount::Here {
            mount_point: PathBuf::from("/mnt"),
        })
    }
    fn swap_active(&self, _disk: &Path, _partition: u32) -> PlatformResult<bool> {
        Ok(false)
    }
    fn resize_partition_live(
        &self,
        _disk: &Path,
        partition: u32,
        start_bytes: u64,
        len_bytes: u64,
    ) -> PlatformResult<()> {
        self.live
            .lock()
            .unwrap()
            .push((partition, start_bytes, len_bytes));
        match self.live_result.lock().unwrap().clone() {
            Ok(()) => Ok(()),
            Err(why) => Err(PlatformError::not_supported(why)),
        }
    }
}

fn image_stamp(bytes: &[u8]) -> [u8; 32] {
    let regions = stamp_regions(bytes.len() as u64, 512);
    let parts: Vec<Vec<u8>> = regions
        .iter()
        .map(|region| bytes[region.start() as usize..region.end() as usize].to_vec())
        .collect();
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    table_stamp(&refs)
}

fn online_execution_plan(bytes: &[u8]) -> diskedit::app::decide::ConfirmedPlan {
    let region = Region::sized(0, 512).expect("表区区间可构造");
    let table = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: 512,
            stamp: image_stamp(bytes),
            segments: vec![TableSegment {
                region,
                bytes: vec![0xAA; 512],
            }],
        },
        vec![region],
    );
    let refresh = Step::new(
        StepKind::RefreshKernelView {
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
            start_bytes: 2048 * 512,
            bytes: 6144 * 512,
        },
        Vec::new(),
    );
    let grow = Step::new(
        StepKind::GrowFilesystemOnline {
            mount_point: PathBuf::from("/mnt"),
            device: PathBuf::from("/dev/sda1"),
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
            filesystem: FsKind::Ext,
            bytes: 6144 * 512,
        },
        Vec::new(),
    );
    let plan = Plan::write(vec![table, refresh, grow], IDENTITY).expect("在线计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

// covers: R0760
#[test]
fn online_resize_writes_the_table_then_resizes_the_kernel_view_then_the_filesystem() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    let platform = LivePlatform::new(
        bytes.clone(),
        vec![
            Call::Exits(Some(0), String::new()),
            Call::Exits(Some(0), geometry(3072, 1024)),
        ],
    );
    let plan = online_execution_plan(&bytes);
    let outcome = resize::run_online(&platform, &plan);
    assert_eq!(outcome.state(), ExecutionState::Committed);
    assert_eq!(exit::exit_of(&outcome), exit::SUCCESS);
    assert_eq!(platform.live_calls(), vec![(1, 2048 * 512, 6144 * 512)]);
    assert_eq!(
        platform.calls(),
        vec![Program::Resize2fs, Program::Dumpe2fs]
    );
}

// covers: R0760
#[test]
fn a_failed_kernel_view_update_after_the_table_write_is_partial() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    let platform = LivePlatform::new(bytes.clone(), Vec::new());
    *platform.live_result.lock().unwrap() = Err("EPERM".to_owned());
    let plan = online_execution_plan(&bytes);
    let outcome = resize::run_online(&platform, &plan);
    assert_eq!(outcome.state(), ExecutionState::CommittedUnsynced);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
    assert!(
        platform.calls().is_empty(),
        "the filesystem step must not run after a failed kernel update"
    );
}

const SHRUNK_BYTES: u64 = 453 * 512;

/// 收缩计划：先缩文件系统，再写表并同步内核视图。
fn online_shrink_execution_plan(bytes: &[u8]) -> diskedit::app::decide::ConfirmedPlan {
    let region = Region::sized(0, 512).expect("表区区间可构造");
    let shrink = Step::new(
        StepKind::ShrinkFilesystemOnline {
            mount_point: PathBuf::from("/mnt"),
            device: PathBuf::from("/dev/sda1"),
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
            filesystem: FsKind::Btrfs,
            bytes: SHRUNK_BYTES,
        },
        Vec::new(),
    );
    let table = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: 512,
            stamp: image_stamp(bytes),
            segments: vec![TableSegment {
                region,
                bytes: vec![0xAA; 512],
            }],
        },
        vec![region],
    );
    let refresh = Step::new(
        StepKind::RefreshKernelView {
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
            start_bytes: 2048 * 512,
            bytes: SHRUNK_BYTES,
        },
        Vec::new(),
    );
    let plan = Plan::write(vec![shrink, table, refresh], IDENTITY).expect("在线计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

// covers: B28, R0742, R0760
#[test]
fn online_btrfs_shrink_shrinks_the_filesystem_before_writing_the_table() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    let platform = LivePlatform::new(
        bytes.clone(),
        vec![
            Call::Exits(Some(0), String::new()),
            Call::Exits(Some(0), superblock(SHRUNK_BYTES, 4096)),
        ],
    );
    let plan = online_shrink_execution_plan(&bytes);
    let outcome = resize::run_online(&platform, &plan);
    assert_eq!(outcome.state(), ExecutionState::Committed);
    assert_eq!(exit::exit_of(&outcome), exit::SUCCESS);
    assert_eq!(platform.calls(), vec![Program::Btrfs, Program::Btrfs]);
    assert_eq!(platform.live_calls(), vec![(1, 2048 * 512, SHRUNK_BYTES)]);
}

// covers: B28, R0742
#[test]
fn a_failed_btrfs_shrink_leaves_the_table_and_kernel_view_alone() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    let platform = LivePlatform::new(bytes.clone(), vec![Call::Exits(Some(1), String::new())]);
    let plan = online_shrink_execution_plan(&bytes);
    let outcome = resize::run_online(&platform, &plan);
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
    assert_eq!(platform.calls(), vec![Program::Btrfs]);
    assert!(
        platform.live_calls().is_empty(),
        "the table must not shrink"
    );
}

// covers: B28, R0742
#[test]
fn a_btrfs_shrink_that_stopped_short_never_shrinks_the_partition() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    let platform = LivePlatform::new(
        bytes.clone(),
        vec![
            Call::Exits(Some(0), String::new()),
            Call::Exits(Some(0), superblock(SHRUNK_BYTES * 2, 4096)),
        ],
    );
    let plan = online_shrink_execution_plan(&bytes);
    let outcome = resize::run_online(&platform, &plan);
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
    assert_eq!(platform.calls(), vec![Program::Btrfs, Program::Btrfs]);
    assert!(
        platform.live_calls().is_empty(),
        "the table must not shrink"
    );
}

// --- swap 重建（B28） -------------------------------------------------------

/// 离线的 swap 分区现场：分区 1 的内容识别为 swap，工具只有 mkswap。
fn swap_snapshot(start: u64, sectors: u64) -> ResizeSnapshot {
    let mut snapshot = online_snapshot(start, sectors);
    snapshot.online = None;
    let content = ContentKind::Swap;
    if let Some(partition) = snapshot.selected.as_mut() {
        partition.content = content;
    }
    if let TableView::Mbr(view) = &mut snapshot.view {
        view.partitions[0].content = content;
    }
    snapshot.tools = vec![(Program::Mkswap, true)];
    snapshot
}

// covers: B28
#[test]
fn resizing_a_swap_partition_plans_a_mkswap_rebuild_after_the_table() {
    let snapshot = swap_snapshot(2048, 1024);
    let request = resize_request(Some(SizeSpec::Grow), None);
    let diskedit::app::decide::Decision::Execute { plan, .. } =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("a swap partition grows by rebuilding it");
    };
    let kinds: Vec<&StepKind> = plan.steps().iter().map(|step| &step.kind).collect();
    assert!(matches!(kinds[0], StepKind::WriteTable { .. }), "{kinds:?}");
    let StepKind::RebuildSwap {
        partition,
        start_bytes,
        bytes,
    } = kinds[1]
    else {
        panic!("swap must be rebuilt after the table takes effect: {kinds:?}");
    };
    assert_eq!(*partition, 1);
    assert_eq!(*start_bytes, 2048 * 512);
    assert_eq!(*bytes, 6144 * 512);
}

// covers: B28
#[test]
fn shrinking_a_swap_partition_is_unsupported() {
    let snapshot = swap_snapshot(2048, 1024);
    let request = resize_request(None, Some(2500));
    let diskedit::app::decide::Decision::Refuse(refusal) =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("swap has no in-place shrink");
    };
    assert_eq!(exit::exit_of_refusal(&refusal), exit::UNSUPPORTED);
}

fn swap_rebuild_plan() -> diskedit::app::decide::ConfirmedPlan {
    let region = Region::sized(0, DISK_BYTES).expect("整盘区间可构造");
    let step = Step::new(
        StepKind::RebuildSwap {
            partition: 0,
            start_bytes: 0,
            bytes: DISK_BYTES,
        },
        vec![region],
    );
    let plan = Plan::write(vec![step], IDENTITY).expect("swap 计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

// covers: B28
#[test]
fn a_swap_rebuild_keeps_the_old_uuid_and_label_on_the_device() {
    let mut image = vec![0u8; DISK_BYTES as usize];
    image[1036..1052].copy_from_slice(&[0x11; 16]);
    image[1052..1056].copy_from_slice(b"SWAP");
    let platform = LivePlatform::new(image, vec![Call::Exits(Some(0), String::new())]);
    let outcome = filesystem::run(&platform, Path::new("/dev/sda"), &swap_rebuild_plan());
    assert_eq!(outcome.state(), ExecutionState::Committed);
    assert_eq!(exit::exit_of(&outcome), exit::SUCCESS);
    assert_eq!(
        platform.arg_calls(),
        vec![(
            Program::Mkswap,
            vec![
                "-U".to_owned(),
                "11111111-1111-1111-1111-111111111111".to_owned(),
                "-L".to_owned(),
                "SWAP".to_owned(),
                "/dev/sda".to_owned(),
            ]
        )]
    );
}

// --- 在线 PV 几何（块设备上的 PV 被 dm 持有） --------------------------------

/// 块设备上的 PV 分区现场：被 device-mapper 持有（占用 `Used`）但未挂载。
fn pv_online_snapshot(start: u64, sectors: u64) -> ResizeSnapshot {
    let mut snapshot = online_snapshot(start, sectors);
    let content = ContentKind::Lvm2Pv;
    snapshot.target.kind = TargetKind::BlockDevice;
    snapshot.occupancy = Occupancy::Used {
        evidence: "held by device-mapper volumes".to_owned(),
    };
    snapshot.online = None;
    snapshot.partition_mount = Some(PartitionMount::NotMounted);
    snapshot.swap_active = Some(false);
    if let Some(partition) = snapshot.selected.as_mut() {
        partition.content = content;
    }
    if let TableView::Mbr(view) = &mut snapshot.view {
        view.partitions[0].content = content;
    }
    snapshot.tools = vec![(Program::Pvs, true), (Program::Pvresize, true)];
    snapshot.lvm = Some(LvmFacts {
        selection: None,
        allocated_end: Ok(0),
    });
    snapshot
}

// covers: B28
#[test]
fn pv_online_grow_plans_table_then_kernel_view_then_pv_chain() {
    let snapshot = pv_online_snapshot(2048, 1024);
    let request = resize_request(Some(SizeSpec::Grow), None);
    let diskedit::app::decide::Decision::Execute { plan, .. } =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("an online PV grow must plan executable steps");
    };
    let kinds: Vec<&StepKind> = plan.steps().iter().map(|step| &step.kind).collect();
    assert!(matches!(kinds[0], StepKind::WriteTable { .. }), "{kinds:?}");
    let StepKind::RefreshKernelView {
        disk,
        partition,
        start_bytes,
        bytes,
    } = kinds[1]
    else {
        panic!("the second step must sync the kernel view: {kinds:?}");
    };
    assert_eq!(disk, &PathBuf::from("/dev/sda"));
    assert_eq!(*partition, 1);
    assert_eq!(*start_bytes, 2048 * 512);
    assert_eq!(*bytes, 6144 * 512);
    let StepKind::LvmGrowChain {
        partition,
        start_bytes,
        old_bytes,
        new_bytes,
        grow_lv,
        ..
    } = kinds[2]
    else {
        panic!("the third step must grow the PV: {kinds:?}");
    };
    assert_eq!(*partition, 1);
    assert_eq!(*start_bytes, 2048 * 512);
    assert_eq!(*old_bytes, 1024 * 512);
    assert_eq!(*new_bytes, 6144 * 512);
    assert!(!*grow_lv);
}

// covers: B28
#[test]
fn pv_online_shrink_is_refused() {
    let snapshot = pv_online_snapshot(2048, 1024);
    let request = resize_request(None, Some(2500));
    let diskedit::app::decide::Decision::Refuse(refusal) =
        diskedit::app::decide::resize::decide(&snapshot, &request)
    else {
        panic!("the online PV path cannot shrink");
    };
    assert_eq!(exit::exit_of_refusal(&refusal), exit::REFUSED);
}

// covers: B28
#[test]
fn pv_online_refuses_move_chunk_and_no_fs() {
    let cases: [fn(&mut ResizeRequest); 3] = [
        |resize| resize.no_fs = true,
        |resize| resize.allow_move = true,
        |resize| resize.chunk = Some(1 << 20),
    ];
    for mutate in cases {
        let snapshot = pv_online_snapshot(2048, 1024);
        let mut resize = resize_command(Some(SizeSpec::Grow), None);
        mutate(&mut resize);
        let request = request_for(resize);
        let diskedit::app::decide::Decision::Refuse(refusal) =
            diskedit::app::decide::resize::decide(&snapshot, &request)
        else {
            panic!("the online PV path must refuse this flag");
        };
        assert_eq!(exit::exit_of_refusal(&refusal), exit::REFUSED);
    }
}

const PV_NEW_BYTES: u64 = 6144 * 512;

fn pv_online_execution_plan(bytes: &[u8]) -> diskedit::app::decide::ConfirmedPlan {
    let table_region = Region::sized(0, 512).expect("表区区间可构造");
    let table = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: 512,
            stamp: image_stamp(bytes),
            segments: vec![TableSegment {
                region: table_region,
                bytes: vec![0xAA; 512],
            }],
        },
        vec![table_region],
    );
    let refresh = Step::new(
        StepKind::RefreshKernelView {
            disk: PathBuf::from("/dev/sda"),
            partition: 1,
            start_bytes: 2048 * 512,
            bytes: PV_NEW_BYTES,
        },
        Vec::new(),
    );
    let lvm_region = Region::sized(2048 * 512, PV_NEW_BYTES).expect("PV 区间可构造");
    let lvm = Step::new(
        StepKind::LvmGrowChain {
            partition: 1,
            start_bytes: 2048 * 512,
            old_bytes: 1024 * 512,
            new_bytes: PV_NEW_BYTES,
            grow_lv: false,
            lv: None,
        },
        vec![lvm_region],
    );
    let plan = Plan::write(vec![table, refresh, lvm], IDENTITY).expect("在线 PV 计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

// covers: B28
#[test]
fn pv_online_runs_the_pv_chain_only_after_the_kernel_view() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    // Pvresize 失败：表与内核视图已成，PV 链失败应归档为待恢复。
    let platform =
        LivePlatform::new_block(bytes.clone(), vec![Call::Exits(Some(1), String::new())]);
    let plan = pv_online_execution_plan(&bytes);
    let outcome = resize::run_pv_online(&platform, &plan);
    assert_eq!(platform.live_calls(), vec![(1, 2048 * 512, PV_NEW_BYTES)]);
    assert_eq!(
        platform.calls(),
        vec![Program::Pvresize],
        "PV 链在内核同步之后才跑"
    );
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
}

// covers: B28
#[test]
fn pv_online_without_kernel_sync_skips_the_pv_chain() {
    let bytes = vec![0u8; DISK_BYTES as usize];
    let platform = LivePlatform::new_block(bytes.clone(), Vec::new());
    *platform.live_result.lock().unwrap() = Err("EPERM".to_owned());
    let plan = pv_online_execution_plan(&bytes);
    let outcome = resize::run_pv_online(&platform, &plan);
    assert_eq!(platform.live_calls(), vec![(1, 2048 * 512, PV_NEW_BYTES)]);
    assert!(platform.calls().is_empty(), "内核视图未同步时不能动 PV");
    assert_eq!(outcome.state(), ExecutionState::CommittedUnsynced);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
}
