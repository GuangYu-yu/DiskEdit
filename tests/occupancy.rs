//! 写表前的占用重探：计划期空闲、执行前被挂载/占用时必须拒绝。
//!
//! 执行器在写表前按计划记录的口径重新探测占用；这里用内存宿主覆盖三种情形：
//! 目标被挂载、所选分区成为活动 swap、以及占用未变时正常落表。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use diskedit::app::decide::step::TableSegment;
use diskedit::app::decide::{
    OccupancyScope, OccupancySnapshot, PartitionUsage, Plan, Step, StepKind, confirm,
};
use diskedit::app::execute::relocate;
use diskedit::app::execute::table;
use diskedit::app::execute::{ExecutionState, FailureKind};
use diskedit::app::observe::TableFormat;
use diskedit::platform::{
    LoopBinding, Occupancy, PartitionMount, Platform, PlatformError, PlatformResult, RecordEntry,
    StateLock, TargetHandle, TargetKind, ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::region::Region;
use diskedit_core::table::stamp::{stamp_regions, table_stamp};

const SECTOR: u64 = 512;
const TOTAL: u64 = 16 * SECTOR;
const IDENTITY: Fingerprint = Fingerprint::strong([0x42; 32]);

struct Lock;
impl StateLock for Lock {}

struct MemoryTarget {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TargetHandle for MemoryTarget {
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
    occupancy: Mutex<Occupancy>,
    mount: Mutex<PartitionMount>,
    swap_active: AtomicBool,
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
}

impl Memory {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(bytes)),
            occupancy: Mutex::new(Occupancy::Idle),
            mount: Mutex::new(PartitionMount::NotMounted),
            swap_active: AtomicBool::new(false),
            files: Mutex::new(BTreeMap::new()),
        }
    }

    fn set_occupancy(&self, occupancy: Occupancy) {
        *self.occupancy.lock().unwrap() = occupancy;
    }

    fn set_swap_active(&self, active: bool) {
        self.swap_active.store(active, Ordering::SeqCst);
    }

    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

impl Platform for Memory {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(MemoryTarget {
            bytes: Arc::clone(&self.bytes),
        }))
    }
    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        self.open_target(_path)
    }
    fn path_kind(&self, _path: &Path) -> TargetKind {
        TargetKind::File
    }
    fn supports_writes(&self) -> bool {
        true
    }
    fn is_partition_node(&self, _path: &Path) -> bool {
        false
    }
    fn identity(&self, _path: &Path) -> PlatformResult<Fingerprint> {
        Ok(IDENTITY)
    }
    fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
        Ok(self.occupancy.lock().unwrap().clone())
    }
    fn partition_mount(&self, _disk: &Path, _partition: u32) -> PlatformResult<PartitionMount> {
        Ok(self.mount.lock().unwrap().clone())
    }
    fn swap_active(&self, _disk: &Path, _partition: u32) -> PlatformResult<bool> {
        Ok(self.swap_active.load(Ordering::SeqCst))
    }
    fn state_root(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/state"))
    }
    fn is_directory(&self, _path: &Path) -> bool {
        false
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
    fn partition_device(&self, _disk: &Path, number: u32) -> PlatformResult<PathBuf> {
        Ok(PathBuf::from(format!("/dev/partition{number}")))
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

/// 一扇区的表写计划，携带给定的占用现场。
fn table_plan(bytes: &[u8], occupancy: OccupancySnapshot) -> diskedit::app::decide::ConfirmedPlan {
    let region = Region::sized(0, SECTOR).expect("表区间");
    let step = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: SECTOR,
            stamp: stamp_of(bytes),
            segments: vec![TableSegment {
                region,
                bytes: vec![0x33; SECTOR as usize],
            }],
        },
        vec![region],
    );
    let plan = Plan::write(vec![step], IDENTITY)
        .expect("写计划需要强身份")
        .with_occupancy(occupancy);
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("确认应成立")
}

fn target_occupancy() -> OccupancySnapshot {
    OccupancySnapshot {
        scope: OccupancyScope::Target,
        occupancy: Occupancy::Idle,
        partition: None,
    }
}

fn partition_occupancy(number: u32) -> OccupancySnapshot {
    OccupancySnapshot {
        scope: OccupancyScope::Target,
        occupancy: Occupancy::Idle,
        partition: Some(PartitionUsage {
            number,
            mount: PartitionMount::NotMounted,
            swap_active: false,
        }),
    }
}

/// 一份搬移计划：数据步在前，表写入步在后，携带给定的占用现场。
fn move_plan(bytes: &[u8], occupancy: OccupancySnapshot) -> diskedit::app::decide::ConfirmedPlan {
    let source_lba = 4;
    let sectors = 4;
    let shift = -4i64;
    let dest_lba = u64::try_from(i128::from(source_lba) + i128::from(shift)).expect("目标 LBA");
    let source = Region::sized(source_lba * SECTOR, sectors * SECTOR).expect("源区间");
    let destination = Region::sized(dest_lba * SECTOR, sectors * SECTOR).expect("目标区间");
    let data = Step::new(
        StepKind::MovePartition {
            partition: 1,
            source_lba,
            offset_sectors: shift,
            chunk: 2 * SECTOR,
            ntfs_hidden: false,
        },
        vec![source, destination],
    );
    let table_region = Region::sized(12 * SECTOR, SECTOR).expect("表区间");
    let table = Step::new(
        StepKind::WriteTable {
            format: TableFormat::Mbr,
            segment_size: SECTOR,
            stamp: stamp_of(bytes),
            segments: vec![TableSegment {
                region: table_region,
                bytes: vec![0x44; SECTOR as usize],
            }],
        },
        vec![table_region],
    );
    let plan = Plan::write(vec![data, table], IDENTITY)
        .expect("写计划需要强身份")
        .with_occupancy(occupancy);
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("确认应成立")
}

#[test]
fn a_target_mounted_after_planning_is_refused_before_the_table_is_written() {
    let bytes = initial_bytes();
    let memory = Memory::new(bytes.clone());
    let plan = table_plan(&bytes, target_occupancy());
    memory.set_occupancy(Occupancy::Used {
        evidence: "mounted from /dev/loop0".to_owned(),
    });

    let outcome = table::run(&memory, Path::new("memory"), &plan);

    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(outcome.failure(), Some(FailureKind::Refused));
    let reason = outcome.reason().expect("拒绝要带成因");
    assert!(reason.contains("usage changed after planning"), "{reason}");
    assert_eq!(memory.bytes(), bytes, "拒绝时不得写目标字节");
}

#[test]
fn a_partition_that_becomes_active_swap_after_planning_is_refused() {
    let bytes = initial_bytes();
    let memory = Memory::new(bytes.clone());
    let plan = table_plan(&bytes, partition_occupancy(1));
    memory.set_swap_active(true);

    let outcome = table::run(&memory, Path::new("memory"), &plan);

    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(outcome.failure(), Some(FailureKind::Refused));
    let reason = outcome.reason().expect("拒绝要带成因");
    assert!(reason.contains("swap usage changed"), "{reason}");
    assert_eq!(memory.bytes(), bytes, "拒绝时不得写目标字节");
}

#[test]
fn unchanged_usage_lets_the_table_write_commit() {
    let bytes = initial_bytes();
    let memory = Memory::new(bytes.clone());
    let plan = table_plan(&bytes, partition_occupancy(1));

    let outcome = table::run(&memory, Path::new("memory"), &plan);

    assert_eq!(
        outcome.state(),
        ExecutionState::Committed,
        "{:?}",
        outcome.reason()
    );
    let written = memory.bytes();
    assert_eq!(&written[..SECTOR as usize], &[0x33; SECTOR as usize]);
}

#[test]
fn a_move_refuses_when_the_target_becomes_busy_after_planning() {
    let bytes = initial_bytes();
    let memory = Memory::new(bytes.clone());
    let plan = move_plan(&bytes, target_occupancy());
    memory.set_occupancy(Occupancy::Used {
        evidence: "mounted from /dev/loop0".to_owned(),
    });

    let outcome = relocate::run(&memory, Path::new("memory"), &plan);

    assert_eq!(outcome.state(), ExecutionState::Unwritten);
    assert_eq!(outcome.failure(), Some(FailureKind::Refused));
    let reason = outcome.reason().expect("拒绝要带成因");
    assert!(reason.contains("usage changed after planning"), "{reason}");
    assert_eq!(memory.bytes(), bytes, "拒绝时不得写目标字节");
}
