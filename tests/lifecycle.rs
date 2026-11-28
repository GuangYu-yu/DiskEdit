//! 生命周期用例：外部工具失败后的终态归档，以及分区表候选的回退。
//!
//! 两条用例都用内存宿主驱动真实执行器与观察层，不触碰真实设备。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diskedit::app::decide::{ConfirmedPlan, Plan, Step, StepKind, confirm};
use diskedit::app::execute::{ExecutionState, filesystem};
use diskedit::app::observe::table::{TableView, guid_text, observe};
use diskedit::app::report::exit;
use diskedit::platform::{
    LoopBinding, Occupancy, Platform, PlatformError, PlatformResult, RecordEntry, StateLock,
    TargetHandle, TargetKind, ToolRun,
};
use diskedit_core::evidence::{Generation, Observation, Source};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;
use diskedit_core::record::{self, FrameKind};
use diskedit_core::region::Region;
use diskedit_core::table::Guid;
use diskedit_core::table::create::gpt_content;

const IDENTITY: Fingerprint = Fingerprint::strong([0x11; 32]);

const DISK_BYTES: u64 = 8192 * 512;

struct Lock;
impl StateLock for Lock {}

/// 镜像句柄：读写都落在内存镜像上。
struct ImageHandle {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TargetHandle for ImageHandle {
    fn path(&self) -> &Path {
        Path::new("/dev/sda")
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
        let start = usize::try_from(offset).expect("偏移可表示");
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
        let start = usize::try_from(offset).expect("偏移可表示");
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

/// 工具调用一律以 `exit_code` 结束的内存宿主。
struct Memory {
    image: Mutex<Vec<u8>>,
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    calls: Mutex<Vec<Program>>,
    exit_code: i32,
}

impl Memory {
    fn new(image: Vec<u8>, exit_code: i32) -> Self {
        Self {
            image: Mutex::new(image),
            files: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            exit_code,
        }
    }

    fn calls(&self) -> Vec<Program> {
        self.calls.lock().unwrap().clone()
    }

    /// 宿主上留下的全部记录字节；本用例最多有一条。
    fn record_bytes(&self) -> Vec<Vec<u8>> {
        self.files.lock().unwrap().values().cloned().collect()
    }
}

impl Platform for Memory {
    fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        Ok(Box::new(ImageHandle {
            bytes: Arc::new(Mutex::new(self.image.lock().unwrap().clone())),
        }))
    }

    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        self.open_target(path)
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

    fn run(&self, program: Program, _args: &[String]) -> PlatformResult<ToolRun> {
        self.calls.lock().unwrap().push(program);
        Ok(ToolRun {
            code: Some(self.exit_code),
            stdout: String::new(),
            stderr: "injected tool failure".to_owned(),
        })
    }

    fn tool_available(&self, _program: Program) -> bool {
        true
    }

    fn tool_name(&self, _program: Program) -> &'static str {
        "mkswap"
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

    fn lock_state(&self, _path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        Ok(Box::new(Lock))
    }
}

/// 整盘上的 swap 分区：`start_bytes` 起的分区按 `bytes` 重建。
fn swap_plan(start_bytes: u64, bytes: u64) -> ConfirmedPlan {
    let region = Region::sized(start_bytes, bytes).expect("swap 区间可构造");
    let step = Step::new(
        StepKind::RebuildSwap {
            partition: 0,
            start_bytes,
            bytes,
        },
        vec![region],
    );
    let plan = Plan::write(vec![step], IDENTITY).expect("swap 计划可构造");
    let current = Observation::known(IDENTITY, Source::new("test"), Generation::FIRST);
    confirm(plan, true, &current).expect("计划可确认")
}

#[test]
fn a_failed_swap_rebuild_is_not_success_and_leaves_no_completed_marker() {
    // 旧 swap 首部：UUID 与卷标都要被带进重建命令。
    let mut image = vec![0u8; usize::try_from(DISK_BYTES).expect("镜像尺寸可表示")];
    image[1036..1052].copy_from_slice(&[0x22; 16]);
    image[1052..1056].copy_from_slice(b"SWAP");
    let platform = Memory::new(image, 1);

    let outcome = filesystem::run(&platform, Path::new("/dev/sda"), &swap_plan(0, DISK_BYTES));

    assert!(!outcome.succeeded(), "工具失败不得报成功：{outcome:?}");
    assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
    assert_eq!(exit::exit_of(&outcome), exit::PARTIAL);
    assert_eq!(platform.calls(), vec![Program::Mkswap], "只跑一次重建");

    // 记录必须还在，且没有任何帧被标成"已完成"：重建步骤没有收尾。
    let records = platform.record_bytes();
    assert_eq!(records.len(), 1, "失败后应留下待恢复记录");
    let decoded = record::decode(&records[0]).expect("记录应可解码");
    assert!(
        decoded.frames.iter().all(|frame| !frame.completed),
        "工具失败的重建不得留下已完成的不可逆标记：{:?}",
        decoded.frames
    );
    assert!(
        decoded
            .frames
            .last()
            .is_some_and(|frame| frame.kind != FrameKind::Committed),
        "失败的重建不得落提交帧：{:?}",
        decoded.frames
    );
}

/// 可注入读取失败的镜像句柄：`fail_offsets` 里的偏移返回 I/O 错误。
struct FailingReads {
    bytes: Vec<u8>,
    fail_offsets: Vec<u64>,
}

impl TargetHandle for FailingReads {
    fn path(&self) -> &Path {
        Path::new("image.img")
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn kind(&self) -> TargetKind {
        TargetKind::File
    }

    fn sector_size(&self) -> Option<u64> {
        Some(4096)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        if self.fail_offsets.contains(&offset) {
            return Err(PlatformError::io(
                self.path(),
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
            ));
        }
        let start = usize::try_from(offset).expect("偏移可表示");
        if start >= self.bytes.len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(self.bytes.len());
        let len = end - start;
        buf[..len].copy_from_slice(&self.bytes[start..end]);
        Ok(len)
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> PlatformResult<usize> {
        Err(PlatformError::not_supported("read-only image"))
    }

    fn barrier(&mut self) -> PlatformResult<()> {
        Ok(())
    }

    fn refresh_partitions(&mut self) -> PlatformResult<()> {
        Ok(())
    }
}

/// 4096 字节逻辑块的整盘 GPT 镜像。
fn gpt_image(total_bytes: u64, sector_size: u64, disk_guid: Guid) -> Vec<u8> {
    let content = gpt_content(total_bytes, sector_size, disk_guid).expect("GPT 内容可生成");
    let mut bytes = vec![0u8; usize::try_from(total_bytes).expect("镜像尺寸可表示")];
    for segment in &content.segments {
        let start = usize::try_from(segment.region.start()).expect("起点可表示");
        let end = usize::try_from(segment.region.end()).expect("终点可表示");
        bytes[start..end].copy_from_slice(&segment.bytes);
    }
    bytes
}

#[test]
fn a_failed_gpt_candidate_falls_through_to_the_next_sector_size() {
    const TOTAL: u64 = 1 << 20;
    const SECTOR: u64 = 4096;
    let disk_guid = Guid([0x5A; 16]);
    let bytes = gpt_image(TOTAL, SECTOR, disk_guid);

    // 512 字节候选读 LBA 1（偏移 512）失败；4096 字节候选读偏移 4096，内容有效。
    let handle = FailingReads {
        bytes: bytes.clone(),
        fail_offsets: vec![512],
    };
    let view = observe(&handle, None, None).expect("第二个候选应读出 GPT");
    let TableView::Gpt(view) = view else {
        panic!("应识别为 GPT：{view:?}");
    };
    assert_eq!(view.sector_size, SECTOR);
    assert_eq!(view.disk_guid, guid_text(disk_guid));
    assert!(view.partitions.is_empty(), "空 GPT 没有分区");

    // 全部候选的首个读取都失败：错误要逐个列出候选与原因。
    let handle = FailingReads {
        bytes,
        fail_offsets: vec![512, 4096, 2048, 1024],
    };
    let error = observe(&handle, None, None).expect_err("全部候选失败必须报错");
    assert!(
        matches!(error, PlatformError::Io { .. }),
        "读取失败的汇总应归入 I/O 错误：{error:?}"
    );
    let text = error.to_string();
    for candidate in ["512-byte sectors", "4096-byte sectors", "2048-byte sectors"] {
        assert!(
            text.contains(candidate),
            "错误里缺少候选 {candidate}：{text}"
        );
    }
}
