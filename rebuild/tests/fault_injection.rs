//! 故障注入：用内存宿主驱动搬移执行器，覆盖写/刷盘/回读/记录落盘的失败分支。
//!
//! 新版把全部目标写入收进唯一写通道（`app::execute::write`），搬移执行器
//! （`app::execute::relocate`）在其上先后做数据步与表步。这里用可注入失败的
//! 内存宿主把旧版 `execution/relocate.rs`、`execution/table.rs` 的构造点接到
//! 同一批失败面：写前失败落 4，写过之后的失败落 5。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use diskedit::app::decide::step::TableSegment;
use diskedit::app::decide::{Plan, Step, StepKind, confirm};
use diskedit::app::execute::relocate;
use diskedit::app::execute::{ExecutionState, Outcome};
use diskedit::app::observe::TableFormat;
use diskedit::app::observe::records::{RECORD_FILE, hex16};
use diskedit::app::report::exit::{ENVIRONMENT, PARTIAL, SUCCESS, UNSUPPORTED, exit_of};
use diskedit::platform::{
    LoopBinding, Occupancy, Platform, PlatformError, PlatformResult, StateLock, TargetHandle,
    TargetKind, ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{FrameKind, decode};
use diskedit_core::region::Region;
use diskedit_core::table::stamp::{stamp_regions, table_stamp};

const SECTOR: u64 = 512;
const TOTAL: u64 = 16 * SECTOR;
const IDENTITY: Fingerprint = Fingerprint::strong([0x42; 32]);

/// 一次运行里注入的失败位置（调用序号从 1 起）。
#[derive(Clone, Copy, Default)]
struct Faults {
    fail_read: Option<usize>,
    fail_write: Option<usize>,
    corrupt_write: Option<usize>,
    fail_barrier: Option<usize>,
    fail_state_write: Option<usize>,
}

#[derive(Default)]
struct Counts {
    reads: AtomicUsize,
    writes: AtomicUsize,
    barriers: AtomicUsize,
    state_writes: AtomicUsize,
}

struct Lock;
impl StateLock for Lock {}

struct FaultTarget {
    bytes: Arc<Mutex<Vec<u8>>>,
    faults: Arc<Faults>,
    counts: Arc<Counts>,
    sector: Option<u64>,
}

impl TargetHandle for FaultTarget {
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
        self.sector
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        let call = self.counts.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if self.faults.fail_read == Some(call) {
            return Err(PlatformError::parse("injected read failure"));
        }
        let bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let len = buf.len().min(bytes.len() - start);
        buf[..len].copy_from_slice(&bytes[start..start + len]);
        Ok(len)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        let call = self.counts.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if self.faults.fail_write == Some(call) {
            return Err(PlatformError::parse("injected write failure"));
        }
        let mut bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > bytes.len() {
            return Err(PlatformError::parse("write past the target"));
        }
        bytes[start..end].copy_from_slice(buf);
        if self.faults.corrupt_write == Some(call) {
            // 写入声称成功，但盘面被外力改掉：回读核对必须发现摘要不符。
            bytes[start] ^= 0xFF;
        }
        Ok(buf.len())
    }
    fn barrier(&mut self) -> PlatformResult<()> {
        let call = self.counts.barriers.fetch_add(1, Ordering::SeqCst) + 1;
        if self.faults.fail_barrier == Some(call) {
            return Err(PlatformError::parse("injected flush failure"));
        }
        Ok(())
    }
    fn refresh_partitions(&mut self) -> PlatformResult<()> {
        Ok(())
    }
}

struct Memory {
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    target: Arc<Mutex<Vec<u8>>>,
    faults: Arc<Faults>,
    counts: Arc<Counts>,
    sector: Option<u64>,
}

impl Memory {
    fn new(bytes: Vec<u8>, faults: Faults) -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            target: Arc::new(Mutex::new(bytes)),
            faults: Arc::new(faults),
            counts: Arc::new(Counts::default()),
            sector: Some(SECTOR),
        }
    }

    /// 以指定的内核扇区尺寸回报构造宿主；`None` 表示普通文件（无内核值）。
    fn with_sector_size(bytes: Vec<u8>, sector: u64, faults: Faults) -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            target: Arc::new(Mutex::new(bytes)),
            faults: Arc::new(faults),
            counts: Arc::new(Counts::default()),
            sector: Some(sector),
        }
    }
}

impl Platform for Memory {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(FaultTarget {
            bytes: Arc::clone(&self.target),
            faults: Arc::clone(&self.faults),
            counts: Arc::clone(&self.counts),
            sector: self.sector,
        }))
    }
    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(FaultTarget {
            bytes: Arc::clone(&self.target),
            faults: Arc::clone(&self.faults),
            counts: Arc::clone(&self.counts),
            sector: self.sector,
        }))
    }
    fn path_kind(&self, _path: &Path) -> TargetKind {
        TargetKind::File
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
    fn is_directory(&self, _path: &Path) -> bool {
        true
    }
    fn list_files(&self, _root: &Path) -> PlatformResult<Vec<PathBuf>> {
        Ok(Vec::new())
    }
    fn read_file(&self, path: &Path) -> PlatformResult<Option<Vec<u8>>> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }
    fn run(
        &self,
        _program: diskedit_core::fs::Program,
        _args: &[String],
    ) -> PlatformResult<ToolRun> {
        Err(PlatformError::not_supported("no tools"))
    }
    fn tool_available(&self, _program: diskedit_core::fs::Program) -> bool {
        false
    }
    fn tool_name(&self, _program: diskedit_core::fs::Program) -> &'static str {
        "unavailable"
    }
    fn bind_loop(&self, _path: &Path, _offset: u64, _len: u64) -> PlatformResult<LoopBinding> {
        Err(PlatformError::not_supported("no loop"))
    }
    fn partition_device(&self, _disk: &Path, _number: u32) -> PlatformResult<PathBuf> {
        Err(PlatformError::not_supported("no partitions"))
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
        Err(PlatformError::not_supported("no temp"))
    }
    fn write_state_file(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        let call = self.counts.state_writes.fetch_add(1, Ordering::SeqCst) + 1;
        if self.faults.fail_state_write == Some(call) {
            return Err(PlatformError::parse("injected record write failure"));
        }
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }
    fn append_state_file(&self, _path: &Path, _bytes: &[u8]) -> PlatformResult<()> {
        Ok(())
    }
    fn remove_state_file(&self, path: &Path) -> PlatformResult<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }
    fn sync_directory(&self, _path: &Path) -> PlatformResult<()> {
        Ok(())
    }
    fn lock_state(&self, _path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        Ok(Box::new(Lock))
    }
}

fn initial_bytes() -> Vec<u8> {
    (0..TOTAL).map(|i| (i % 251) as u8 ^ 0x5a).collect()
}

fn stamp_of(bytes: &[u8]) -> [u8; 32] {
    let parts: Vec<Vec<u8>> = stamp_regions(TOTAL, SECTOR)
        .into_iter()
        .map(|region| bytes[region.start() as usize..region.end() as usize].to_vec())
        .collect();
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    table_stamp(&refs)
}

/// 一份左移两扇区的搬移计划，外加一个单扇区的表写入步。
fn move_plan(stamp: [u8; 32]) -> diskedit::app::decide::ConfirmedPlan {
    let source = Region::sized(2 * SECTOR, 4 * SECTOR).expect("源区间");
    let destination = Region::sized(0, 4 * SECTOR).expect("目标区间");
    let data = Step::new(
        StepKind::MovePartition {
            partition: 1,
            source_lba: 2,
            offset_sectors: -2,
            chunk: 2 * SECTOR,
            ntfs_hidden: false,
        },
        vec![source, destination],
    );

    let table_region = Region::sized(8 * SECTOR, SECTOR).expect("表区间");
    let table = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: SECTOR,
            stamp,
            segments: vec![TableSegment {
                region: table_region,
                bytes: vec![0x33; SECTOR as usize],
            }],
        },
        vec![table_region],
    );

    let plan = Plan::write(vec![data, table], IDENTITY).expect("写计划需要强身份");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("确认应成立")
}

fn run(faults: Faults) -> (Outcome, Memory) {
    let bytes = initial_bytes();
    let stamp = stamp_of(&bytes);
    let memory = Memory::new(bytes, faults);
    let plan = move_plan(stamp);
    let outcome = relocate::run(&memory, Path::new("memory"), &plan);
    (outcome, memory)
}

fn assert_partial(outcome: &Outcome, what: &str) {
    assert_eq!(
        outcome.state(),
        ExecutionState::NeedsRecovery,
        "{what}: {:?}",
        outcome.reason()
    );
    assert_eq!(exit_of(outcome), PARTIAL, "{what}");
}

// covers: R0577
#[test]
fn a_failed_source_read_changes_nothing_and_reports_no_mutation() {
    // 前三次读取用于写前表戳（首/次/末扇区），第四次是数据步的源读。
    let (outcome, memory) = run(Faults {
        fail_read: Some(4),
        ..Faults::default()
    });
    assert_eq!(
        outcome.state(),
        ExecutionState::Unwritten,
        "{:?}",
        outcome.reason()
    );
    assert_eq!(exit_of(&outcome), ENVIRONMENT);
    // 源读失败发生在首次写之前，目标字节不变。
    assert_eq!(*memory.target.lock().unwrap(), initial_bytes());
}

// covers: R0578
#[test]
fn a_write_failure_after_the_first_block_reports_partial() {
    // 第一块写完（目标已被改过），第二块写失败：应报待恢复。
    let (outcome, _memory) = run(Faults {
        fail_write: Some(2),
        ..Faults::default()
    });
    assert_partial(&outcome, "数据写入失败");
}

// covers: R0579, R0580, R0581
#[test]
fn a_failed_data_flush_reports_partial() {
    let (outcome, _memory) = run(Faults {
        fail_barrier: Some(1),
        ..Faults::default()
    });
    assert_partial(&outcome, "数据刷盘失败");
}

// covers: R0582
#[test]
fn data_that_does_not_read_back_reports_partial() {
    let (outcome, _memory) = run(Faults {
        corrupt_write: Some(1),
        ..Faults::default()
    });
    assert_partial(&outcome, "回读不符");
}

// covers: R0583
#[test]
fn a_failed_progress_checkpoint_reports_partial() {
    // 第一次状态写入是意图帧；第二次是数据块的进度帧。
    let (outcome, _memory) = run(Faults {
        fail_state_write: Some(2),
        ..Faults::default()
    });
    assert_partial(&outcome, "进度记录落盘失败");
}

// covers: R0591
#[test]
fn a_failed_table_write_after_the_move_reports_partial() {
    // 前两次写入是数据块，第三次是表写。
    let (outcome, _memory) = run(Faults {
        fail_write: Some(3),
        ..Faults::default()
    });
    assert_partial(&outcome, "表写失败");
}

// covers: R0593
#[test]
fn a_failed_table_flush_reports_partial() {
    // 第一次刷盘是数据步，第二次是表步。
    let (outcome, _memory) = run(Faults {
        fail_barrier: Some(2),
        ..Faults::default()
    });
    assert_partial(&outcome, "表刷盘失败");
}

/// 记录文件路径：与执行器一致的 `<state>/images/<hex16>/record`。
fn record_path() -> PathBuf {
    PathBuf::from("/state")
        .join("images")
        .join(hex16(IDENTITY))
        .join(RECORD_FILE)
}

fn read_record(memory: &Memory) -> Vec<u8> {
    memory
        .files
        .lock()
        .unwrap()
        .get(&record_path())
        .cloned()
        .expect("失败后记录必须保留")
}

// covers: B6
#[test]
fn a_relocation_interruption_leaves_only_table_preimages_and_keeps_old_source() {
    // 第二块写失败：数据步已写过第一块，记录里只应有意图帧与进度帧，
    // 不应出现任何数据区原像；被搬走的源区此后仍保留旧字节（数据步不回滚）。
    let (outcome, memory) = run(Faults {
        fail_write: Some(2),
        ..Faults::default()
    });
    assert_partial(&outcome, "搬移中断");

    let decoded = decode(&read_record(&memory)).expect("记录应可解码");
    assert!(
        decoded
            .frames
            .iter()
            .all(|frame| frame.kind != FrameKind::Preimage),
        "中断在数据步时不应登记任何原像：{:?}",
        decoded.frames
    );

    // 源区 LBA2..6（字节 1024..3072）不参与回滚，旧字节原样保留。
    let target = memory.target.lock().unwrap();
    let initial = initial_bytes();
    assert_eq!(&target[1024..3072], &initial[1024..3072], "旧源区不得清零");
}

// covers: B16
#[test]
fn the_table_preimage_is_recorded_before_the_table_write() {
    // 第三次写是表步：写失败时记录里必须已经有覆盖表区间的原像帧。
    let (outcome, memory) = run(Faults {
        fail_write: Some(3),
        ..Faults::default()
    });
    assert_partial(&outcome, "表写失败");

    let decoded = decode(&read_record(&memory)).expect("记录应可解码");
    let covered = decoded.frames.iter().any(|frame| {
        frame.kind == FrameKind::Preimage
            && frame.regions.iter().any(|region| {
                region.offset <= 8 * SECTOR && 9 * SECTOR <= region.offset + region.len
            })
    });
    assert!(covered, "表写前必须已落表区原像：{:?}", decoded.frames);
}

#[test]
fn a_clean_run_commits_and_cleans_the_record() {
    let (outcome, memory) = run(Faults::default());
    assert_eq!(
        outcome.state(),
        ExecutionState::Committed,
        "{:?}",
        outcome.reason()
    );
    assert_eq!(exit_of(&outcome), SUCCESS);
    assert!(
        memory.files.lock().unwrap().is_empty(),
        "提交后记录应被清理"
    );
}

/// 以假端口跑一条完整命令行；用于驱动需要特定内核事实（扇区尺寸、占用、身份）的分支。
fn dispatch(memory: &Memory, argv: &[&str]) -> u8 {
    let args: Vec<std::ffi::OsString> = argv.iter().map(std::ffi::OsString::from).collect();
    diskedit::app::cli::dispatch_with(memory, args)
}

// covers: R0004, R0014, R0018, R0021, R0023, R0024, R0026, R0029
// covers: R0036
// covers: B1, B19
#[test]
fn a_target_with_an_unsupported_sector_size_is_refused_as_unsupported() {
    // 内核回报 520 字节扇区（不在 512..=65536 的 2 的幂集合内）：全部 LBA 换算失去
    // 依据，写命令必须按能力不支持拒绝（退 6），而不是静默当作 512。
    let memory = Memory::with_sector_size(vec![0u8; 8192], 520, Faults::default());
    let code = dispatch(&memory, &["diskedit", "add", "memory", "1M", "--yes"]);
    assert_eq!(code, UNSUPPORTED, "集合外扇区尺寸应按 unsupported 拒绝");
}
