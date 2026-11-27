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
use diskedit::app::execute::table;
use diskedit::app::execute::{ExecutionState, Outcome};
use diskedit::app::observe::TableFormat;
use diskedit::app::observe::records::{RECORD_FILE, hex16};
use diskedit::app::report::exit::{ENVIRONMENT, PARTIAL, REFUSED, SUCCESS, UNSUPPORTED, exit_of};
use diskedit::platform::{
    LoopBinding, Occupancy, PartitionMount, Platform, PlatformError, PlatformResult, RecordEntry,
    StateLock, TargetHandle, TargetKind, ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{FrameKind, RecoveryPlan, decode, recover};
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
    fail_state_remove: Option<usize>,
}

#[derive(Default)]
struct Counts {
    reads: AtomicUsize,
    writes: AtomicUsize,
    barriers: AtomicUsize,
    state_writes: AtomicUsize,
    state_removes: AtomicUsize,
}

struct Lock;
impl StateLock for Lock {}

struct FaultTarget {
    bytes: Arc<Mutex<Vec<u8>>>,
    faults: Arc<Faults>,
    counts: Arc<Counts>,
    sector: Option<u64>,
    kind: TargetKind,
}

impl TargetHandle for FaultTarget {
    fn path(&self) -> &Path {
        Path::new("memory")
    }
    fn len(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn kind(&self) -> TargetKind {
        self.kind
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
    kind: TargetKind,
    /// 目标自身能否给出 Strong 身份；块设备无 WWN/EUI/可重复序列号时为 `false`。
    strong_identity: bool,
    /// `list_files` 回报的条目，用于驱动记录目录扫描。
    scan_files: Vec<PathBuf>,
    /// 让 `read_file` 以读故障结束，用于记录读取失败的语义。
    read_failure: bool,
    /// 让 `identity` 以读故障结束，用于身份探测失败的语义。
    identity_failure: bool,
}

impl Memory {
    fn new(bytes: Vec<u8>, faults: Faults) -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            target: Arc::new(Mutex::new(bytes)),
            faults: Arc::new(faults),
            counts: Arc::new(Counts::default()),
            sector: Some(SECTOR),
            kind: TargetKind::File,
            strong_identity: true,
            scan_files: Vec::new(),
            read_failure: false,
            identity_failure: false,
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
            ..Self::new(Vec::new(), Faults::default())
        }
    }

    /// 以"块设备但无自身稳定身份"的形态回报：`--alias` 在这里才适用。
    fn weak_block_device(bytes: Vec<u8>) -> Self {
        Self {
            target: Arc::new(Mutex::new(bytes)),
            kind: TargetKind::BlockDevice,
            strong_identity: false,
            ..Self::new(Vec::new(), Faults::default())
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
            kind: self.kind,
        }))
    }
    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(FaultTarget {
            bytes: Arc::clone(&self.target),
            faults: Arc::clone(&self.faults),
            counts: Arc::clone(&self.counts),
            sector: self.sector,
            kind: self.kind,
        }))
    }
    fn path_kind(&self, _path: &Path) -> TargetKind {
        self.kind
    }
    fn filesystem_ready(&self, _path: &Path) -> bool {
        self.kind == TargetKind::BlockDevice
    }
    fn is_partition_node(&self, _path: &Path) -> bool {
        false
    }
    fn identity(&self, path: &Path) -> PlatformResult<Fingerprint> {
        if self.identity_failure {
            return Err(PlatformError::io(
                path,
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
        }
        if self.strong_identity {
            Ok(IDENTITY)
        } else {
            Ok(Fingerprint::unusable([0u8; 32]))
        }
    }
    fn partition_mount(&self, _disk: &Path, _partition: u32) -> PlatformResult<PartitionMount> {
        Ok(PartitionMount::NotMounted)
    }
    fn swap_active(&self, _disk: &Path, _partition: u32) -> PlatformResult<bool> {
        Ok(false)
    }
    fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
        Ok(Occupancy::Idle)
    }
    fn state_root(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/state"))
    }
    fn is_directory(&self, _path: &Path) -> bool {
        self.kind == TargetKind::File
    }
    fn list_files(&self, _root: &Path) -> PlatformResult<Vec<PathBuf>> {
        Ok(self.scan_files.clone())
    }
    fn read_file(&self, path: &Path) -> PlatformResult<Option<Vec<u8>>> {
        if self.read_failure {
            return Err(PlatformError::io(
                path,
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
        }
        Ok(self.files.lock().unwrap().get(path).cloned())
    }
    fn read_record(&self, path: &Path) -> PlatformResult<RecordEntry> {
        if self.read_failure {
            return Err(PlatformError::io(
                path,
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
        }
        Ok(match self.files.lock().unwrap().get(path).cloned() {
            Some(bytes) => RecordEntry::Contents(bytes),
            None => RecordEntry::Absent,
        })
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
        let call = self.counts.state_removes.fetch_add(1, Ordering::SeqCst) + 1;
        if self.faults.fail_state_remove == Some(call) {
            return Err(PlatformError::parse("injected record removal failure"));
        }
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

// covers: R0591, R0883
#[test]
fn a_failed_table_write_after_the_move_reports_partial() {
    // 前两次写入是数据块，第三次是表写。
    let (outcome, _memory) = run(Faults {
        fail_write: Some(3),
        ..Faults::default()
    });
    assert_partial(&outcome, "表写失败");
}

// covers: R0593, R0884
#[test]
fn a_failed_table_flush_reports_partial() {
    // 第一次刷盘是数据步，第二次是表步。
    let (outcome, _memory) = run(Faults {
        fail_barrier: Some(2),
        ..Faults::default()
    });
    assert_partial(&outcome, "表刷盘失败");
}

// covers: R0886
#[test]
fn a_table_that_changes_after_the_write_reports_partial() {
    // 第三次写是表步；写入声称成功但盘面被外力改掉：回读核对必须发现摘要不符，
    // 按"表已落盘、待恢复"报部分完成，而不是当作成功。
    let (outcome, _memory) = run(Faults {
        corrupt_write: Some(3),
        ..Faults::default()
    });
    assert_partial(&outcome, "表回读不符");
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

/// 一块 4 MiB、单分区 (LBA 2048 + 1024)、无自身稳定身份的块设备镜像字节。
fn weak_block_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 8192 * SECTOR as usize];
    bytes[446 + 1..446 + 4].copy_from_slice(&[0x01, 0x01, 0x00]);
    bytes[446 + 4] = 0x83;
    bytes[446 + 5..446 + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    bytes[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
    bytes[446 + 12..446 + 16].copy_from_slice(&1024u32.to_le_bytes());
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    bytes
}

// covers: I2, I5, D25
#[test]
fn a_write_naming_its_own_identity_on_an_unidentified_block_device_is_confirmed() {
    // 块设备没有 WWN/EUI/可重复序列号时，`--alias NAME` 就是它的身份。确认阶段
    // 必须用这个声明名做比较，而不是只在自动身份为 Strong 时才算数——否则所有带
    // `--alias` 的块设备写入都会被误判成"身份不可证明"。
    let memory = Memory::weak_block_device(weak_block_bytes());
    let code = dispatch(
        &memory,
        &[
            "diskedit",
            "resize",
            "memory:1",
            "--grow-to-end",
            "--no-fs",
            "--alias",
            "lab-disk",
            "--yes",
        ],
    );
    assert_eq!(code, SUCCESS, "带声明身份的块设备写入应确认并执行");
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

// covers: I15
#[test]
fn a_record_that_cannot_be_read_is_an_environment_failure() {
    // 记录目录里有一条以记录文件命名的条目，但读取失败：不能当作"没有记录"
    // 跳过并报告扫描完整，必须让失败上行。
    let mut memory = Memory::new(vec![0u8; 4096], Faults::default());
    memory.scan_files = vec![PathBuf::from("/root/images/key").join(RECORD_FILE)];
    memory.read_failure = true;
    let code = dispatch(&memory, &["diskedit", "records", "list", "--root", "/root"]);
    assert_eq!(code, ENVIRONMENT, "读取记录失败必须按环境失败上报");
}

// covers: I15
#[test]
fn an_identity_that_cannot_be_read_is_not_reported_as_no_identity() {
    // 身份探测读失败不能折成"这个目标没有身份"：那会让 `--alias` 变成兜底，
    // 把读不出来的身份当成可声明的空位继续写。观察阶段的读故障按拒绝终止。
    let mut memory = Memory::new(vec![0u8; 4096], Faults::default());
    memory.identity_failure = true;
    let code = dispatch(&memory, &["diskedit", "layout", "memory"]);
    assert_eq!(code, REFUSED, "身份读失败必须拒绝，不能当作没有身份");
}

// covers: I14
#[test]
fn a_grow_chain_naming_a_smaller_target_than_its_recorded_size_is_invalid() {
    // 决策层只会产出变大的 PV 尺寸。执行边界若看到 new < old，计划就自相矛盾：
    // 不能把差值按 0 处理再照常 pvresize，那会静默把 PV 缩小。
    let region = Region::sized(0, SECTOR).expect("区间可构造");
    let step = Step::new(
        StepKind::LvmGrowChain {
            partition: 1,
            start_bytes: 0,
            old_bytes: 2 * 1024 * 1024,
            new_bytes: 1024 * 1024,
            grow_lv: false,
            lv: None,
        },
        vec![region],
    );
    let plan = Plan::write(vec![step], IDENTITY).expect("计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    let plan = confirm(plan, true, &current).expect("计划可确认");
    let outcome = diskedit::app::execute::lvm::run(
        &diskedit::platform::UnsupportedPlatform,
        Path::new("memory"),
        &plan,
    );
    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    let reason = outcome.reason().unwrap_or_default();
    assert!(
        reason.contains("smaller than"),
        "自相矛盾的 grow 计划应按无效计划拒绝，实际：{reason}"
    );
}

// covers: D10
#[test]
fn a_host_without_a_backend_refuses_with_the_unsupported_code() {
    // 未接入的宿主端口对每个能力都返回不支持；上层必须落 6，而不是当作目标拒绝。
    let args: Vec<std::ffi::OsString> = ["diskedit", "records", "show", "x.img"]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    let code = diskedit::app::cli::dispatch_with(&diskedit::platform::UnsupportedPlatform, args);
    assert_eq!(code, UNSUPPORTED);
}

/// 只含一次表写的已确认计划；表区就是 LBA0。
fn table_only_plan(bytes: &[u8]) -> diskedit::app::decide::ConfirmedPlan {
    let region = Region::sized(0, SECTOR).expect("表区区间可构造");
    let step = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: SECTOR,
            stamp: stamp_of(bytes),
            segments: vec![TableSegment {
                region,
                bytes: vec![0x11u8; SECTOR as usize],
            }],
        },
        vec![region],
    );
    let plan = Plan::write(vec![step], IDENTITY).expect("表计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

// covers: R0042, R0043
#[test]
fn a_target_that_changed_since_the_plan_is_not_written() {
    // 计划确认后盘面被外力改动：表区摘要不再一致，复用该计划必须按"未写"拒绝，
    // 而不是在已变的盘上重写。
    let bytes = initial_bytes();
    let plan = table_only_plan(&bytes);
    let memory = Memory::new(bytes, Faults::default());
    *memory.target.lock().unwrap().get_mut(0).expect("首字节") ^= 0x01;
    let outcome = table::run(&memory, Path::new("memory"), &plan);
    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(exit_of(&outcome), REFUSED);
}

// covers: R0958
#[test]
fn a_target_smaller_than_one_sector_is_refused_before_writing() {
    let bytes = initial_bytes();
    let plan = table_only_plan(&bytes);
    let memory = Memory::new(vec![0u8; 256], Faults::default());
    let outcome = table::run(&memory, Path::new("memory"), &plan);
    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(exit_of(&outcome), ENVIRONMENT);
}

// covers: R0536
#[test]
fn a_plan_without_a_table_write_step_never_touches_the_target() {
    let step = Step::new(StepKind::Inspect, Vec::new());
    let plan = Plan::write(vec![step], IDENTITY).expect("只读步骤也可装入写计划");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    let plan = confirm(plan, true, &current).expect("计划可确认");
    let memory = Memory::new(initial_bytes(), Faults::default());
    let outcome = table::run(&memory, Path::new("memory"), &plan);
    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(exit_of(&outcome), REFUSED);
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        initial_bytes().as_slice(),
        "计划不含表写时不得改动目标"
    );
}

// covers: F1
#[test]
fn a_commit_that_cannot_clear_its_record_leaves_a_terminal_record() {
    // 提交先写终态帧、再删除记录。若在两者之间崩溃，盘上的记录必须自证"已提交"，
    // 恢复器才会只清理记录；否则最后一条完整帧仍是原像帧，恢复会把已提交的写入回滚。
    let bytes = initial_bytes();
    let plan = table_only_plan(&bytes);
    let memory = Memory::new(
        bytes,
        Faults {
            fail_state_remove: Some(1),
            ..Faults::default()
        },
    );
    let _ = table::run(&memory, Path::new("memory"), &plan);
    let path = PathBuf::from("/state")
        .join("images")
        .join(hex16(IDENTITY))
        .join(RECORD_FILE);
    let record = memory
        .files
        .lock()
        .unwrap()
        .get(&path)
        .cloned()
        .expect("清理失败后记录应留在原处");
    assert_eq!(
        recover(&record, IDENTITY),
        RecoveryPlan::Committed,
        "删除记录前盘上必须已经是完整的已提交记录"
    );
}

/// 与 `relocate::spec_bytes` 相同的几何快照（测试侧独立构造，用来喂记录）。
fn resume_spec() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"diskedit-relocate-v1");
    out.push(1);
    for value in [2u64, 4, 0, 2 * SECTOR, SECTOR, TOTAL] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

// covers: B24
#[test]
fn a_resumed_move_with_a_mismatched_prefix_is_refused_without_writing() {
    let bytes = initial_bytes();
    let stamp = stamp_of(&bytes);
    let memory = Memory::new(bytes.clone(), Faults::default());
    let plan = move_plan(stamp);
    let mut progress = 1u64.to_le_bytes().to_vec();
    progress.extend_from_slice(&[0u8; 16]);
    let record = diskedit_core::record::encode(
        &diskedit_core::record::RecordBody {
            target: IDENTITY,
            geometry_generation: 0,
            plan_digest: plan.digest(),
        },
        &[
            diskedit_core::record::Frame {
                seq: 0,
                kind: FrameKind::Intent,
                completed: false,
                irreversible: false,
                intent: resume_spec(),
                regions: Vec::new(),
            },
            diskedit_core::record::Frame {
                seq: 1,
                kind: FrameKind::Progress,
                completed: false,
                irreversible: true,
                intent: progress,
                regions: Vec::new(),
            },
        ],
    )
    .expect("记录可编码");
    let path = PathBuf::from("/state")
        .join("images")
        .join(hex16(IDENTITY))
        .join(RECORD_FILE);
    memory.files.lock().unwrap().insert(path, record);
    let outcome = relocate::run(&memory, Path::new("memory"), &plan);
    assert_eq!(
        outcome.state(),
        ExecutionState::Unwritten,
        "{:?}",
        outcome.reason()
    );
    assert_eq!(exit_of(&outcome), REFUSED);
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        bytes.as_slice(),
        "前缀不符时不得写目标"
    );
}

// covers: F8
#[test]
fn a_pending_record_for_another_target_is_not_adopted() {
    // 记录目录按身份派生，但记录本身的 `body.target` 仍可能指向别的载体（例如
    // 从另一台机器恢复目录、或记录被替换）。主写路径不得只看"末帧未终结"就接管：
    // 目标指纹不符时必须拒绝，并且不得改动目标字节或删除这条记录。
    let bytes = initial_bytes();
    let plan = table_only_plan(&bytes);
    let memory = Memory::new(bytes.clone(), Faults::default());
    let record = diskedit_core::record::encode(
        &diskedit_core::record::RecordBody {
            target: Fingerprint::strong([0x99; 32]),
            geometry_generation: 0,
            plan_digest: plan.request_digest(),
        },
        &[diskedit_core::record::Frame {
            seq: 0,
            kind: FrameKind::Intent,
            completed: false,
            irreversible: false,
            intent: b"write-table".to_vec(),
            regions: Vec::new(),
        }],
    )
    .expect("记录可编码");
    let path = PathBuf::from("/state")
        .join("images")
        .join(hex16(IDENTITY))
        .join(RECORD_FILE);
    memory
        .files
        .lock()
        .unwrap()
        .insert(path.clone(), record.clone());
    let outcome = table::run(&memory, Path::new("memory"), &plan);
    assert_eq!(
        outcome.state(),
        ExecutionState::Unwritten,
        "{:?}",
        outcome.reason()
    );
    assert_eq!(exit_of(&outcome), REFUSED);
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        bytes.as_slice(),
        "不得把别的载体的记录当作本目标的原像"
    );
    assert_eq!(
        memory.files.lock().unwrap().get(&path),
        Some(&record),
        "拒绝后记录应原样保留"
    );
}
