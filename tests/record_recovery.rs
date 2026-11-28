//! 搬移执行器在内存宿主上的盘面断言：NTFS `HiddenSectors` 与自重叠搬移顺序。
//!
//! 宿主只实现端口必需项；记录位置读写落在一张内存表里，目标字节由一块共享
//! 缓冲区表示，因此可以直接核对最终盘面。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diskedit::app::decide::step::TableSegment;
use diskedit::app::decide::{ConfirmedPlan, Plan, Step, StepKind, confirm};
use diskedit::app::execute::relocate;
use diskedit::app::execute::{ExecutionState, FailureKind};
use diskedit::app::observe::TableFormat;
use diskedit::platform::{
    LoopBinding, Occupancy, Platform, PlatformError, PlatformResult, RecordEntry, StateLock,
    TargetHandle, TargetKind, ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{
    Frame, FrameKind, ManualReason, RecordBody, RecoveryExpectation, RecoveryPlan, decode, encode,
    length_generation, recover,
};
use diskedit_core::region::Region;
use diskedit_core::table::relocate::layout;
use diskedit_core::table::stamp::{stamp_regions, table_stamp};

const SECTOR: u64 = 512;
const TOTAL: u64 = 64 * SECTOR;
/// 表写入步占用的扇区，避开测试里所有数据区间。
const TABLE_LBA: u64 = 48;
const IDENTITY: Fingerprint = Fingerprint::strong([0x42; 32]);

struct Lock;

impl StateLock for Lock {}

struct Target {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TargetHandle for Target {
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
        Some(SECTOR)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
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
        let mut bytes = self.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > bytes.len() {
            return Err(PlatformError::parse("write past the target"));
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

struct Memory {
    bytes: Arc<Mutex<Vec<u8>>>,
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    /// 每次记录写入的字节，按写入顺序保存。
    record_writes: Mutex<Vec<Vec<u8>>>,
}

impl Memory {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(bytes)),
            files: Mutex::new(BTreeMap::new()),
            record_writes: Mutex::new(Vec::new()),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    fn record_writes(&self) -> Vec<Vec<u8>> {
        self.record_writes.lock().unwrap().clone()
    }
}

impl Platform for Memory {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(Target {
            bytes: Arc::clone(&self.bytes),
        }))
    }

    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(Target {
            bytes: Arc::clone(&self.bytes),
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

    fn list_record_entries(&self, _root: &Path, _name: &str) -> PlatformResult<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    fn read_record(&self, path: &Path) -> PlatformResult<RecordEntry> {
        Ok(match self.files.lock().unwrap().get(path).cloned() {
            Some(bytes) => RecordEntry::Contents(bytes),
            None => RecordEntry::Absent,
        })
    }

    fn write_record(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), bytes.to_vec());
        self.record_writes.lock().unwrap().push(bytes.to_vec());
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

    fn lock_state(&self, _path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        Ok(Box::new(Lock))
    }
}

fn stamp_of(bytes: &[u8]) -> [u8; 32] {
    let parts: Vec<Vec<u8>> = stamp_regions(TOTAL, SECTOR)
        .into_iter()
        .map(|region| bytes[region.start() as usize..region.end() as usize].to_vec())
        .collect();
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    table_stamp(&refs)
}

/// 一份搬移计划：数据步在前，表写入步在后。
fn move_plan(
    stamp: [u8; 32],
    source_lba: u64,
    sectors: u64,
    shift: i64,
    ntfs_hidden: bool,
) -> ConfirmedPlan {
    let dest_lba = u64::try_from(source_lba as i128 + shift as i128).expect("目标 LBA");
    let source = Region::sized(source_lba * SECTOR, sectors * SECTOR).expect("源区间");
    let destination = Region::sized(dest_lba * SECTOR, sectors * SECTOR).expect("目标区间");
    let data = Step::new(
        StepKind::MovePartition {
            partition: 1,
            source_lba,
            offset_sectors: shift,
            chunk: 4 * SECTOR,
            ntfs_hidden,
        },
        vec![source, destination],
    );

    let table_region = Region::sized(TABLE_LBA * SECTOR, SECTOR).expect("表区间");
    let table = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: SECTOR,
            stamp,
            segments: vec![TableSegment {
                region: table_region,
                bytes: vec![0xa5; SECTOR as usize],
            }],
        },
        vec![table_region],
    );

    let plan = Plan::write(vec![data, table], IDENTITY).expect("写计划需要强身份");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("确认应成立")
}

/// 一组合成扇区：每个扇区内容不同，便于对照搬移结果。
fn filled_sectors(sectors: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((sectors * SECTOR) as usize);
    for index in 0..sectors {
        let fill = u8::try_from(index).expect("扇区号可表示") + 0x10;
        bytes.extend(std::iter::repeat_n(fill, SECTOR as usize));
    }
    bytes
}

/// 一段 NTFS 引导扇区：OEM 标识、卷总扇区数、新起点字段与引导标记齐备。
fn ntfs_boot_sector(hidden: u32, total_sectors: u64, fill: u8) -> Vec<u8> {
    let mut sector = vec![fill; SECTOR as usize];
    sector[..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
    sector[3..11].copy_from_slice(b"NTFS    ");
    sector[0x1c..0x20].copy_from_slice(&hidden.to_le_bytes());
    sector[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    sector[510..512].copy_from_slice(&[0x55, 0xaa]);
    sector
}

/// 把 NTFS 引导扇区放到区间首末扇区；其余扇区保留填充字节。
fn install_ntfs_boot(source: &mut [u8], sectors: u64, hidden: u32) {
    let first = ntfs_boot_sector(hidden, sectors, source[0]);
    source[..SECTOR as usize].copy_from_slice(&first);
    let last_at = ((sectors - 1) * SECTOR) as usize;
    let last = ntfs_boot_sector(hidden, sectors, source[last_at]);
    source[last_at..last_at + SECTOR as usize].copy_from_slice(&last);
}

#[test]
fn an_ntfs_move_rewrites_hidden_sectors_in_both_boot_sectors() {
    let source_lba = 8;
    let sectors = 16;
    let mut bytes = filled_sectors(TOTAL / SECTOR);
    let source_at = (source_lba * SECTOR) as usize;
    let source_len = (sectors * SECTOR) as usize;
    install_ntfs_boot(
        &mut bytes[source_at..source_at + source_len],
        sectors,
        u32::try_from(source_lba).expect("源起点可表示"),
    );
    let before = bytes[source_at..source_at + source_len].to_vec();
    assert_eq!(
        &before[0x1c..0x20],
        &8u32.to_le_bytes(),
        "合成引导扇区应记录旧起点"
    );

    let stamp = stamp_of(&bytes);
    let memory = Memory::new(bytes);
    // 左移 8 扇区：源与目标自重叠，按核心布局从头搬。
    let plan = move_plan(stamp, source_lba, sectors, -(source_lba as i64), true);
    let outcome = relocate::run(&memory, Path::new("memory"), &plan);
    assert!(outcome.succeeded(), "搬移应完成：{:?}", outcome.reason());

    let mut expected = before;
    expected[0x1c..0x20].copy_from_slice(&0u32.to_le_bytes());
    let last_at = ((sectors - 1) * SECTOR) as usize + 0x1c;
    expected[last_at..last_at + 4].copy_from_slice(&0u32.to_le_bytes());

    let disk = memory.bytes();
    let dest = &disk[..source_len];
    assert_eq!(
        dest,
        expected.as_slice(),
        "搬移结果必须等于源内容，仅新起点字段被改写"
    );
    assert_eq!(
        &disk[(TABLE_LBA * SECTOR) as usize..((TABLE_LBA + 1) * SECTOR) as usize],
        &[0xa5; SECTOR as usize],
        "表写入步必须落盘"
    );
}

#[test]
fn a_self_overlapping_move_keeps_order_and_content_for_both_signs() {
    let sectors = 16;
    let chunk_lba = 4;
    for shift in [8i64, -8] {
        let source_lba = if shift > 0 { 0 } else { 8 };
        let dest_lba = u64::try_from(source_lba as i128 + shift as i128).expect("目标 LBA");
        let bytes = filled_sectors(TOTAL / SECTOR);
        let source_at = (source_lba * SECTOR) as usize;
        let source_len = (sectors * SECTOR) as usize;
        let before = bytes[source_at..source_at + source_len].to_vec();

        // 顺序由核心布局按位移符号给出：正位移尾块在前，负位移头块在前。
        let spans = layout(sectors, SECTOR, shift, chunk_lba * SECTOR)
            .expect("布局")
            .spans(source_lba);
        if shift > 0 {
            assert_eq!(spans[0].first_lba, source_lba + sectors - chunk_lba);
            assert_eq!(spans.last().expect("有块").first_lba, source_lba);
        } else {
            assert_eq!(spans[0].first_lba, source_lba);
            assert_eq!(
                spans.last().expect("有块").first_lba,
                source_lba + sectors - chunk_lba
            );
        }

        let stamp = stamp_of(&bytes);
        let memory = Memory::new(bytes);
        let plan = move_plan(stamp, source_lba, sectors, shift, false);
        let outcome = relocate::run(&memory, Path::new("memory"), &plan);
        assert!(
            outcome.succeeded(),
            "位移 {shift} 的搬移应完成：{:?}",
            outcome.reason()
        );
        let disk = memory.bytes();
        let dest_at = (dest_lba * SECTOR) as usize;
        assert_eq!(
            &disk[dest_at..dest_at + source_len],
            before.as_slice(),
            "位移 {shift} 的自重叠搬移必须保持内容"
        );
    }
}

#[test]
fn an_ntfs_move_without_ntfs_boot_sectors_writes_nothing() {
    let source_lba = 8;
    let sectors = 16;
    let bytes = filled_sectors(TOTAL / SECTOR);
    let before = bytes.clone();
    let stamp = stamp_of(&bytes);
    let memory = Memory::new(bytes);
    let plan = move_plan(stamp, source_lba, sectors, -(source_lba as i64), true);
    let outcome = relocate::run(&memory, Path::new("memory"), &plan);
    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(outcome.failure(), Some(FailureKind::Refused));
    assert_eq!(memory.bytes(), before, "签名不符时目标字节不得改变");
}

/// 走一次完成的自重叠搬移，返回这次运行写下的第一份记录字节。
fn first_written_record() -> Vec<u8> {
    let source_lba = 8;
    let sectors = 16;
    let bytes = filled_sectors(TOTAL / SECTOR);
    let stamp = stamp_of(&bytes);
    let memory = Memory::new(bytes);
    let plan = move_plan(stamp, source_lba, sectors, -(source_lba as i64), false);
    let outcome = relocate::run(&memory, Path::new("memory"), &plan);
    assert!(outcome.succeeded(), "搬移应完成：{:?}", outcome.reason());
    memory
        .record_writes()
        .into_iter()
        .next()
        .expect("搬移开始前必须先落一条记录")
}

#[test]
fn a_written_record_carries_the_length_generation_of_the_target() {
    let record = first_written_record();
    let decoded = decode(&record).expect("记录应可解码");
    assert_ne!(decoded.body.length_generation, 0, "写入端不得留下零代次");
    assert_eq!(
        decoded.body.length_generation,
        length_generation(TOTAL),
        "记录里的长度代次必须等于按目标总字节数算出的值"
    );
}

#[test]
fn a_record_whose_target_length_changed_is_refused() {
    let record = first_written_record();
    assert!(
        !matches!(
            recover(&record, RecoveryExpectation::new(IDENTITY, TOTAL)),
            RecoveryPlan::NeedsManual { .. }
        ),
        "长度未变时记录应按帧前缀给出结论"
    );
    let plan = recover(&record, RecoveryExpectation::new(IDENTITY, TOTAL + SECTOR));
    let RecoveryPlan::NeedsManual { why } = plan else {
        panic!("长度改动后必须拒绝，实际：{plan:?}");
    };
    assert_eq!(why, ManualReason::GenerationMismatch);
}

#[test]
fn a_record_for_another_target_with_the_same_geometry_is_refused_by_identity() {
    let record = first_written_record();
    let other = Fingerprint::strong([0x99; 32]);
    let plan = recover(&record, RecoveryExpectation::new(other, TOTAL));
    let RecoveryPlan::NeedsManual { why } = plan else {
        panic!("别的载体即使几何相同也必须拒绝，实际：{plan:?}");
    };
    assert_eq!(why, ManualReason::DifferentTarget);
}

#[test]
fn a_record_without_a_length_generation_is_not_replayed() {
    let body = RecordBody {
        target: IDENTITY,
        length_generation: 0,
        plan_digest: [0x11; 32],
    };
    let frames = [Frame {
        seq: 0,
        kind: FrameKind::Progress,
        completed: true,
        irreversible: false,
        intent: Vec::new(),
        regions: Vec::new(),
    }];
    let record = encode(&body, &frames).expect("记录应可编码");
    let plan = recover(&record, RecoveryExpectation::new(IDENTITY, TOTAL));
    assert!(
        !matches!(plan, RecoveryPlan::Replay { .. }),
        "代次为零的记录不得回放"
    );
    let RecoveryPlan::NeedsManual { why } = plan else {
        panic!("代次为零的记录必须拒绝，实际：{plan:?}");
    };
    assert_eq!(why, ManualReason::GenerationMismatch);
}
