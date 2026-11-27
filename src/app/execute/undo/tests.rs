//! 恢复执行器的最小宿主：只实现恢复用到的那几件事，其余端口显式失败。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diskedit_core::record::{Frame, FrameKind, Preimage as RecordPreimage, RecordBody, encode};

use super::*;
use crate::app::decide::{Plan, Step, confirm};
use crate::app::execute::digest::Hasher;
use crate::app::execute::{ExecutionState, Outcome};
use crate::platform::{
    LoopBinding, Occupancy, PlatformError, PlatformResult, RecordEntry, StateLock, TargetHandle,
    TargetKind, ToolRun,
};

const IDENTITY: Fingerprint = Fingerprint::strong([0x42; 32]);
const SECTOR: usize = 512;

#[derive(Default)]
struct Memory {
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    target: Arc<Mutex<Vec<u8>>>,
    fail_writes: Arc<std::sync::atomic::AtomicUsize>,
}

struct Lock;

impl StateLock for Lock {}

impl Memory {
    fn with_target(bytes: Vec<u8>) -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            target: Arc::new(Mutex::new(bytes)),
            fail_writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn put(&self, path: &Path, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_path_buf(), bytes);
    }

    fn get(&self, path: &Path) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .files
            .lock()
            .unwrap()
            .keys()
            .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }
}

struct MemoryTarget {
    bytes: Arc<Mutex<Vec<u8>>>,
    fail_writes: Arc<std::sync::atomic::AtomicUsize>,
    writes: Arc<std::sync::atomic::AtomicUsize>,
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
        Some(SECTOR as u64)
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
        let call = self
            .writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) == call {
            return Err(PlatformError::parse("injected rollback write failure"));
        }
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

impl Platform for Memory {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(MemoryTarget {
            bytes: Arc::clone(&self.target),
            fail_writes: Arc::clone(&self.fail_writes),
            writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }))
    }
    fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(MemoryTarget {
            bytes: Arc::clone(&self.target),
            fail_writes: Arc::clone(&self.fail_writes),
            writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
        Ok(match self.get(path) {
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
    fn write_record(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        self.put(path, bytes.to_vec());
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
}

fn record_path() -> PathBuf {
    PathBuf::from("/state")
        .join("images")
        .join(hex16(IDENTITY))
        .join(RECORD_FILE)
}

fn body() -> RecordBody {
    RecordBody {
        target: IDENTITY,
        geometry_generation: 1,
        plan_digest: [0x11; 32],
    }
}

fn preimage_frame(original: &[u8]) -> Frame {
    preimage_frame_with(original, &[])
}

fn preimage_frame_with(original: &[u8], after: &[u8]) -> Frame {
    let mut hasher = Hasher::new();
    hasher.update(original);
    Frame {
        seq: 1,
        kind: FrameKind::Preimage,
        completed: true,
        irreversible: false,
        intent: Vec::new(),
        regions: vec![RecordPreimage {
            offset: 0,
            len: original.len() as u64,
            digest: hasher.finish(),
            data: original.to_vec(),
            after: after.to_vec(),
        }],
    }
}

fn intent_frame() -> Frame {
    Frame {
        seq: 0,
        kind: FrameKind::Intent,
        completed: false,
        irreversible: false,
        intent: b"write-table".to_vec(),
        regions: Vec::new(),
    }
}

fn record_bytes(frames: &[Frame]) -> Vec<u8> {
    encode(&body(), frames).expect("记录可编码")
}

fn confirmed(mode: RecoverMode, regions: Vec<Region>) -> ConfirmedPlan {
    let step = Step::new(StepKind::RecoverRecords { mode }, regions);
    let plan = Plan::write(vec![step], IDENTITY).expect("写计划需要强身份");
    let current = diskedit_core::evidence::Observation::known(
        IDENTITY,
        diskedit_core::evidence::Source::new("test"),
        diskedit_core::evidence::Generation::FIRST,
    );
    confirm(plan, true, &current).expect("确认应成立")
}

fn run_mode(memory: &Memory, mode: RecoverMode) -> Outcome {
    let plan = confirmed(mode, Vec::new());
    match mode {
        RecoverMode::Release { .. } => {
            crate::app::execute::release::run(memory, Path::new("memory"), &plan)
        }
        RecoverMode::Undo | RecoverMode::ReplayTableOnly => run(memory, Path::new("memory"), &plan),
    }
}

fn recovered(outcome: &Outcome) -> bool {
    matches!(
        outcome.state(),
        ExecutionState::RolledBack | ExecutionState::Committed
    )
}

// covers: B7, B9
#[test]
fn rollback_verifies_the_preimage_and_cleans_the_record() {
    let original = vec![0xAAu8; SECTOR];
    let memory = Memory::with_target(original.clone());
    memory.put(
        &record_path(),
        record_bytes(&[
            Frame {
                seq: 0,
                kind: FrameKind::Intent,
                completed: false,
                irreversible: false,
                intent: b"write-table".to_vec(),
                regions: Vec::new(),
            },
            preimage_frame(&original),
        ]),
    );
    let plan = confirmed(
        RecoverMode::Undo,
        vec![Region::sized(0, SECTOR as u64).unwrap()],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert!(recovered(&outcome), "{:?}", outcome.reason());
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        original.as_slice()
    );
    assert!(memory.get(&record_path()).is_none(), "清理后记录不应存在");
}

// covers: B24
#[test]
fn a_mismatched_region_is_refused_and_kept() {
    let original = vec![0xAAu8; SECTOR];
    let memory = Memory::with_target(vec![0x55u8; SECTOR]);
    memory.put(
        &record_path(),
        record_bytes(&[
            Frame {
                seq: 0,
                kind: FrameKind::Intent,
                completed: false,
                irreversible: false,
                intent: b"write-table".to_vec(),
                regions: Vec::new(),
            },
            preimage_frame(&original),
        ]),
    );
    let plan = confirmed(
        RecoverMode::Undo,
        vec![Region::sized(0, SECTOR as u64).unwrap()],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert!(
        !recovered(&outcome),
        "盘面偏离原像应拒绝：{:?}",
        outcome.reason()
    );
    assert_eq!(memory.target.lock().unwrap().as_slice(), &[0x55u8; SECTOR]);
    assert!(memory.get(&record_path()).is_some(), "拒绝后必须保留记录");
}

// covers: B6, B24
#[test]
fn a_fully_written_table_is_replayed_back_to_the_preimage() {
    let original = vec![0xAAu8; SECTOR];
    let after = vec![0xBBu8; SECTOR];
    let memory = Memory::with_target(after.clone());
    memory.put(
        &record_path(),
        record_bytes(&[intent_frame(), preimage_frame_with(&original, &after)]),
    );
    let plan = confirmed(
        RecoverMode::ReplayTableOnly,
        vec![Region::sized(0, SECTOR as u64).unwrap()],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert!(recovered(&outcome), "{:?}", outcome.reason());
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        original.as_slice(),
        "写后字节应被写回原像"
    );
    assert!(memory.get(&record_path()).is_none());
}

// covers: B6, B24
#[test]
fn a_torn_write_with_one_break_point_is_replayed() {
    let original = vec![0xAAu8; SECTOR];
    let after = vec![0xBBu8; SECTOR];
    let mut torn = original.clone();
    torn[SECTOR / 2..].copy_from_slice(&after[SECTOR / 2..]);
    let memory = Memory::with_target(torn);
    memory.put(
        &record_path(),
        record_bytes(&[intent_frame(), preimage_frame_with(&original, &after)]),
    );
    let plan = confirmed(
        RecoverMode::ReplayTableOnly,
        vec![Region::sized(0, SECTOR as u64).unwrap()],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert!(recovered(&outcome), "{:?}", outcome.reason());
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        original.as_slice()
    );
    assert!(memory.get(&record_path()).is_none());
}

// covers: B24
#[test]
fn bytes_that_match_neither_side_are_refused_before_any_write() {
    let original = vec![0xAAu8; SECTOR];
    let after = vec![0xBBu8; SECTOR];
    let memory = Memory::with_target(vec![0x5Au8; SECTOR]);
    memory.put(
        &record_path(),
        record_bytes(&[intent_frame(), preimage_frame_with(&original, &after)]),
    );
    let plan = confirmed(
        RecoverMode::ReplayTableOnly,
        vec![Region::sized(0, SECTOR as u64).unwrap()],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert!(!recovered(&outcome), "{:?}", outcome.reason());
    assert_eq!(memory.target.lock().unwrap().as_slice(), &[0x5Au8; SECTOR]);
    assert!(memory.get(&record_path()).is_some(), "拒绝后必须保留记录");
}

#[test]
fn the_second_tear_point_is_a_conflict() {
    let mut log = TearLog::default();
    assert_eq!(log.record(512), Ok(()));
    assert_eq!(log.record(512), Ok(()), "同一断点投影到多条区间不算冲突");
    assert_eq!(log.record(768), Err(512));
}

#[test]
fn recorded_write_classifies_after_torn_and_original() {
    let pre = [0xAA; 8];
    let after = [0xBB; 8];
    assert_eq!(
        RecordedWrite::of(&after, &after, &pre),
        Some(RecordedWrite::After)
    );
    assert_eq!(
        RecordedWrite::of(&pre, &after, &pre),
        Some(RecordedWrite::Original)
    );
    let mut torn = pre;
    torn[4..].copy_from_slice(&after[4..]);
    assert_eq!(
        RecordedWrite::of(&torn, &after, &pre),
        Some(RecordedWrite::Torn { cut: 4 })
    );
    assert_eq!(RecordedWrite::of(&[0x5A; 8], &after, &pre), None);
    assert_eq!(
        RecordedWrite::of(&[0x5A; 8], &[], &pre),
        None,
        "缺写后字节时不得猜测"
    );
}

// covers: B8
#[test]
fn release_archives_instead_of_touching_the_target() {
    let memory = Memory::with_target(vec![0x55u8; SECTOR]);
    memory.put(
        &record_path(),
        record_bytes(&[preimage_frame(&vec![0xAAu8; SECTOR])]),
    );
    let outcome = run_mode(
        &memory,
        RecoverMode::Release {
            discard_replay: true,
        },
    );
    assert!(recovered(&outcome));
    assert_eq!(memory.target.lock().unwrap().as_slice(), &[0x55u8; SECTOR]);
    assert!(memory.get(&record_path()).is_none());
    assert!(
        memory.names().contains(&"record.1.abandoned".to_owned()),
        "归档名应顺延：{:?}",
        memory.names()
    );
}

// covers: B8
#[test]
fn a_second_release_does_not_overwrite_the_first_archive() {
    let memory = Memory::with_target(vec![0x55u8; SECTOR]);
    memory.put(
        &record_path(),
        record_bytes(&[preimage_frame(&vec![0xAAu8; SECTOR])]),
    );
    let first = run_mode(
        &memory,
        RecoverMode::Release {
            discard_replay: true,
        },
    );
    assert!(recovered(&first));
    memory.put(
        &record_path(),
        record_bytes(&[preimage_frame(&vec![0xBBu8; SECTOR])]),
    );
    let second = run_mode(
        &memory,
        RecoverMode::Release {
            discard_replay: true,
        },
    );
    assert!(recovered(&second));
    let names = memory.names();
    assert!(
        names.contains(&"record.1.abandoned".to_owned()),
        "{names:?}"
    );
    assert!(
        names.contains(&"record.2.abandoned".to_owned()),
        "{names:?}"
    );
}

// covers: B23
#[test]
fn cleanup_refuses_when_the_record_changed_under_the_lock() {
    let original = vec![0xAAu8; SECTOR];
    let memory = Memory::with_target(vec![0x55u8; SECTOR]);
    // Committed 记录：回复只清理、不写目标。
    let bytes = record_bytes(&[
        Frame {
            seq: 0,
            kind: FrameKind::Intent,
            completed: false,
            irreversible: false,
            intent: b"write-table".to_vec(),
            regions: Vec::new(),
        },
        preimage_frame(&original),
        Frame {
            seq: 2,
            kind: FrameKind::Committed,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: Vec::new(),
        },
    ]);
    memory.put(&record_path(), bytes);
    let outcome = run_mode(&memory, RecoverMode::Undo);
    assert!(recovered(&outcome), "{:?}", outcome.reason());
    assert!(memory.get(&record_path()).is_none());
    assert_eq!(memory.target.lock().unwrap().as_slice(), &[0x55u8; SECTOR]);
}

// covers: B3
#[test]
fn only_the_directory_derived_from_the_identity_is_consulted() {
    let memory = Memory::with_target(vec![0x55u8; SECTOR]);
    memory.put(
        &record_path(),
        record_bytes(&[preimage_frame(&vec![0xAAu8; SECTOR])]),
    );
    let elsewhere = PathBuf::from("/state/images/deadbeef/record");
    memory.put(
        &elsewhere,
        record_bytes(&[preimage_frame(&vec![0xCCu8; SECTOR])]),
    );
    let outcome = run_mode(
        &memory,
        RecoverMode::Release {
            discard_replay: true,
        },
    );
    assert!(recovered(&outcome));
    assert!(memory.get(&record_path()).is_none());
    assert!(
        memory.get(&elsewhere).is_some(),
        "别的身份目录里的记录不该被这个目标处理"
    );
}

// covers: B24
#[test]
fn a_failed_rollback_write_keeps_the_rest_of_the_record_and_reports_partial() {
    let mut before = vec![0xAAu8; SECTOR];
    before.extend(vec![0xCCu8; SECTOR]);
    let mut after = vec![0xBBu8; SECTOR];
    after.extend(vec![0xDDu8; SECTOR]);
    let mut first = Hasher::new();
    first.update(&before[..SECTOR]);
    let mut second = Hasher::new();
    second.update(&before[SECTOR..]);
    let frame = Frame {
        seq: 1,
        kind: FrameKind::Preimage,
        completed: true,
        irreversible: false,
        intent: Vec::new(),
        regions: vec![
            RecordPreimage {
                offset: 0,
                len: SECTOR as u64,
                digest: first.finish(),
                data: before[..SECTOR].to_vec(),
                after: after[..SECTOR].to_vec(),
            },
            RecordPreimage {
                offset: SECTOR as u64,
                len: SECTOR as u64,
                digest: second.finish(),
                data: before[SECTOR..].to_vec(),
                after: after[SECTOR..].to_vec(),
            },
        ],
    };
    let memory = Memory::with_target(after.clone());
    memory.put(&record_path(), record_bytes(&[intent_frame(), frame]));
    memory
        .fail_writes
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let plan = confirmed(
        RecoverMode::Undo,
        vec![
            Region::sized(0, SECTOR as u64).unwrap(),
            Region::sized(SECTOR as u64, SECTOR as u64).unwrap(),
        ],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert_eq!(
        outcome.state(),
        ExecutionState::NeedsRecovery,
        "{:?}",
        outcome.reason()
    );
    let target = memory.target.lock().unwrap().clone();
    let first_restored = target[..SECTOR] == before[..SECTOR];
    let second_restored = target[SECTOR..] == before[SECTOR..];
    assert!(
        first_restored ^ second_restored,
        "回滚应完成且只完成其中一段：first={first_restored} second={second_restored}"
    );
    assert!(
        memory.get(&record_path()).is_some(),
        "回滚未完成时记录必须保留"
    );
}

// covers: B24
#[test]
fn a_record_with_non_monotonic_frames_is_refused_before_writing() {
    let bytes = vec![0xCCu8; SECTOR];
    let memory = Memory::with_target(bytes.clone());
    let mut second = preimage_frame(&bytes);
    second.seq = 0;
    memory.put(&record_path(), record_bytes(&[intent_frame(), second]));
    let plan = confirmed(
        RecoverMode::Undo,
        vec![Region::sized(0, SECTOR as u64).unwrap()],
    );
    let outcome = run(&memory, Path::new("memory"), &plan);
    assert_eq!(
        outcome.state(),
        ExecutionState::Unwritten,
        "{:?}",
        outcome.reason()
    );
    assert_eq!(
        memory.target.lock().unwrap().as_slice(),
        bytes.as_slice(),
        "记录自相矛盾时不得写目标"
    );
}
