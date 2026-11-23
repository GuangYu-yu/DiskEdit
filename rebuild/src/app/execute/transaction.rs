//! 事务：先登记原像，再写目标，最后提交记录。
//!
//! 顺序固定：读取并摘要本步声明区间 → 记录整份落盘 → 铸造令牌 → 写通道写入 →
//! 提交目标缓存 → 回读核对 → 追加完成帧。任一步失败都落在确定状态上，不会出现
//! "写了一半却报告未改动"。

use std::path::Path;

use diskedit_core::record::{
    Frame, FrameKind, Preimage, RecordBody, RecoveryPlan, encode, recover,
};

use super::digest::hash_region;
use super::state::StateMachine;
use super::steps::{self, Context};
use super::token::Recorded;
use super::write::WriteChannel;
use super::{ExecError, FailureKind, Faults, Outcome};
use crate::app::decide::ConfirmedPlan;
use crate::app::decide::step::{Access, Step, StepKind};
use crate::platform::{Platform, RecordStore, TargetHandle};

/// 单次读取原像的块大小。
const PREIMAGE_CHUNK: usize = 1 << 20;

/// 铸造令牌的必要条件；只能在事务模块内取得实例。
#[derive(Clone, Copy, Debug)]
pub struct Mint(());

impl Mint {
    const fn new() -> Self {
        Self(())
    }
}

/// 执行一份已确认计划。
pub fn run(
    confirmed: &ConfirmedPlan,
    platform: &dyn Platform,
    path: &Path,
    generation: u64,
    faults: Faults,
) -> Outcome {
    let mut handle = match platform.open_target(path) {
        Ok(handle) => handle,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, platform_text(&error));
        }
    };
    if let Err(error) = handle.lock_exclusive() {
        return Outcome::unwritten(FailureKind::FailedBeforeWrite, platform_text(&error));
    }
    let observed = match platform.fingerprint(path) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, platform_text(&error));
        }
    };
    if !confirmed.identity().matches(observed) {
        return Outcome::unwritten(FailureKind::Refused, "持锁后的目标身份与确认时不一致");
    }
    let sector = match handle.sector_size() {
        Some(size) if size > 0 => size,
        _ => return Outcome::unwritten(FailureKind::FailedBeforeWrite, "无法确定目标扇区尺寸"),
    };

    let writes_target = confirmed
        .steps()
        .iter()
        .any(|step| step.access() == Access::WritesTarget);

    if !writes_target {
        return run_read_only(confirmed, platform, path);
    }

    let identity = confirmed.identity();
    let mut store = match platform.records(path, identity) {
        Ok(store) => store,
        Err(error) => {
            return Outcome::unwritten(FailureKind::FailedBeforeWrite, platform_text(&error));
        }
    };
    if let Some(existing) = match store.load() {
        Ok(bytes) => bytes,
        Err(error) => return Outcome::unwritten(FailureKind::Refused, platform_text(&error)),
    } {
        match recover(&existing, identity) {
            RecoveryPlan::Absent | RecoveryPlan::Committed => {
                if let Err(error) = store.clear() {
                    return Outcome::unwritten(FailureKind::Refused, platform_text(&error));
                }
            }
            RecoveryPlan::RollBack { frames } if frames.is_empty() => {
                if let Err(error) = store.clear() {
                    return Outcome::unwritten(FailureKind::Refused, platform_text(&error));
                }
            }
            RecoveryPlan::Foreign { .. } => {
                return Outcome::unwritten(FailureKind::Refused, "记录目录中存在本实现之外的内容");
            }
            RecoveryPlan::RollBack { .. }
            | RecoveryPlan::Replay { .. }
            | RecoveryPlan::NeedsManual { .. } => {
                return Outcome::unwritten(FailureKind::Refused, "存在未完成的记录，需要先恢复");
            }
        }
    }

    let body = RecordBody {
        target: identity,
        geometry_generation: generation,
        plan_digest: confirmed.digest(),
    };
    let digest = confirmed.digest();
    let ctx = Context {
        plan_digest: &digest,
        sector,
    };
    let mut frames: Vec<Frame> = Vec::new();
    let mut machine = StateMachine::new();
    let mut wrote_any = false;

    for step in confirmed.steps() {
        match step.kind.access() {
            Access::ReadOnly => continue,
            Access::WritesExternalState => {
                if let StepKind::RefreshKernelView = step.kind {
                    if let Err(error) = platform.update_table(path) {
                        let reason = platform_text(&error);
                        if wrote_any {
                            if let Err(commit_error) =
                                finish_terminal(&mut *store, &body, &mut frames)
                            {
                                return Outcome::needs_recovery(format!(
                                    "内核视图未同步：{reason}；{commit_error}"
                                ));
                            }
                            if let Err(transition) = machine.commit_unsynced() {
                                return Outcome::needs_recovery(format!(
                                    "状态推进失败: {transition:?}"
                                ));
                            }
                            return Outcome::committed_unsynced(reason);
                        }
                        return Outcome::unwritten(FailureKind::FailedBeforeWrite, reason);
                    }
                } else {
                    return Outcome::unwritten(FailureKind::Unsupported, "记录恢复执行器尚未接入");
                }
            }
            Access::WritesTarget => {
                match execute_target_step(
                    step,
                    &mut *handle,
                    &mut *store,
                    &body,
                    &mut frames,
                    &mut machine,
                    &ctx,
                    &faults,
                ) {
                    Ok(()) => wrote_any = true,
                    Err(failure) => return failure,
                }
            }
        }
    }

    if let Err(error) = finish_terminal(&mut *store, &body, &mut frames) {
        return Outcome::needs_recovery(error);
    }
    if let Err(error) = machine.commit() {
        return Outcome::needs_recovery(format!("状态推进失败: {error:?}"));
    }
    if let Err(error) = store.clear() {
        return Outcome::committed_unsynced(platform_text(&error));
    }
    Outcome::completed()
}

fn run_read_only(confirmed: &ConfirmedPlan, platform: &dyn Platform, path: &Path) -> Outcome {
    for step in confirmed.steps() {
        if let StepKind::RefreshKernelView = step.kind {
            if let Err(error) = platform.update_table(path) {
                return Outcome::unwritten(FailureKind::FailedBeforeWrite, platform_text(&error));
            }
        }
    }
    Outcome::completed()
}

#[allow(clippy::too_many_arguments)]
fn execute_target_step(
    step: &Step,
    handle: &mut dyn TargetHandle,
    store: &mut dyn RecordStore,
    body: &RecordBody,
    frames: &mut Vec<Frame>,
    machine: &mut StateMachine,
    ctx: &Context<'_>,
    faults: &Faults,
) -> Result<(), Outcome> {
    if let Err(error) = steps::precheck(&step.kind, ctx) {
        return Err(classify_unwritten(error));
    }
    if faults.before_write {
        return Err(Outcome::unwritten(
            FailureKind::FailedBeforeWrite,
            "写前中断注入",
        ));
    }

    let mut preimages = Vec::with_capacity(step.regions.len());
    for region in &step.regions {
        let digest = hash_region(handle, *region, PREIMAGE_CHUNK).map_err(classify_unwritten)?;
        preimages.push(Preimage {
            offset: region.start(),
            len: region.len(),
            digest,
        });
    }
    let sequence = frames.len() as u32;
    frames.push(Frame {
        seq: sequence,
        kind: FrameKind::Preimage,
        completed: false,
        intent: intent_of(&step.kind).as_bytes().to_vec(),
        regions: preimages,
    });
    if let Err(error) = store.store(&encode_or_failure(body, frames)?) {
        frames.pop();
        return Err(Outcome::unwritten(
            FailureKind::FailedBeforeWrite,
            platform_text(&error),
        ));
    }
    if let Err(error) = machine.arm() {
        return Err(Outcome::needs_recovery(format!("状态推进失败: {error:?}")));
    }
    let token = Recorded::mint(&Mint::new(), u64::from(sequence));

    let fail_after = if faults.mid_write { Some(1) } else { None };
    let mut channel = WriteChannel::new(handle, &step.regions).with_fail_after(fail_after);
    if let Err(error) = steps::apply(&step.kind, &step.regions, &mut channel, &token, ctx) {
        let reason = match channel.barrier() {
            Ok(()) => format_error(&error),
            Err(barrier) => format!(
                "{}；提交缓存失败：{}",
                format_error(&error),
                format_error(&barrier)
            ),
        };
        return Err(Outcome::needs_recovery(reason));
    }
    if let Err(error) = channel.barrier() {
        return Err(Outcome::needs_recovery(format_error(&error)));
    }
    if let Err(error) = super::postcondition::verify(&channel) {
        return Err(Outcome::needs_recovery(format_error(&error)));
    }
    if faults.after_write {
        return Err(Outcome::needs_recovery("写后中断注入"));
    }
    frames.push(Frame {
        seq: frames.len() as u32,
        kind: FrameKind::Progress,
        completed: true,
        intent: intent_of(&step.kind).as_bytes().to_vec(),
        regions: Vec::new(),
    });
    if let Err(error) = store.store(&encode_or_failure(body, frames)?) {
        return Err(Outcome::needs_recovery(platform_text(&error)));
    }
    Ok(())
}

fn finish_terminal(
    store: &mut dyn RecordStore,
    body: &RecordBody,
    frames: &mut Vec<Frame>,
) -> Result<(), String> {
    frames.push(Frame {
        seq: frames.len() as u32,
        kind: FrameKind::Committed,
        completed: true,
        intent: b"commit".to_vec(),
        regions: Vec::new(),
    });
    let bytes = encode(body, frames).map_err(|error| format!("记录编码失败: {error:?}"))?;
    store.store(&bytes).map_err(|error| platform_text(&error))
}

fn encode_or_failure(body: &RecordBody, frames: &[Frame]) -> Result<Vec<u8>, Outcome> {
    encode(body, frames)
        .map_err(|error| Outcome::needs_recovery(format!("记录编码失败: {error:?}")))
}

fn classify_unwritten(error: ExecError) -> Outcome {
    match error {
        ExecError::Unsupported(why) => Outcome::unwritten(FailureKind::Unsupported, why),
        ExecError::InvalidPlan(why) | ExecError::Record(why) => {
            Outcome::unwritten(FailureKind::Refused, why)
        }
        other @ (ExecError::Platform(_)
        | ExecError::IdentityChanged(_)
        | ExecError::Io(_)
        | ExecError::Postcondition(_)
        | ExecError::Injected(_)) => {
            Outcome::unwritten(FailureKind::FailedBeforeWrite, format_error(&other))
        }
    }
}

fn format_error(error: &ExecError) -> String {
    match error {
        ExecError::Platform(error) => platform_text(error),
        ExecError::Record(why)
        | ExecError::IdentityChanged(why)
        | ExecError::Unsupported(why)
        | ExecError::InvalidPlan(why)
        | ExecError::Io(why)
        | ExecError::Postcondition(why)
        | ExecError::Injected(why) => why.clone(),
    }
}

fn platform_text(error: &crate::platform::PlatformError) -> String {
    error.to_string()
}

fn intent_of(kind: &StepKind) -> &'static str {
    match kind {
        StepKind::WriteTable { .. } => "write-table",
        StepKind::AddPartition { .. } => "add-partition",
        StepKind::DeletePartition { .. } => "delete-partition",
        StepKind::ResizePartition { .. } => "resize-partition",
        StepKind::SetProperty { .. } => "set-property",
        StepKind::MakeFilesystem { .. } => "make-filesystem",
        StepKind::GrowFilesystem { .. } => "grow-filesystem",
        StepKind::ShrinkFilesystem { .. } => "shrink-filesystem",
        StepKind::CopyPartition { .. } => "copy-partition",
        StepKind::MovePartition { .. } => "move-partition",
        StepKind::RefreshKernelView => "refresh-kernel-view",
        StepKind::RecoverRecords => "recover-records",
        StepKind::Inspect => "inspect",
        StepKind::Check { .. } => "check",
        StepKind::ListRecords => "list-records",
        StepKind::VerifyRecord { .. } => "verify-record",
        StepKind::ScanRecords => "scan-records",
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::rc::Rc;

    use diskedit_core::fingerprint::Fingerprint;

    use super::*;
    use crate::app::decide::{Decision, Plan, confirm, decide};
    use crate::app::execute::ExecutionState;
    use crate::app::observe::{Command, Request, Snapshot, TableFormat, observe};
    use crate::platform::{
        MountEntry, Occupancy, Output, Partition, PlatformError, PlatformResult, RecordEntry,
        RecordStore, SwapEntry, TargetKind,
    };
    use diskedit_core::fs::Program;

    const SECTOR: u64 = 512;

    type Image = Rc<RefCell<Vec<u8>>>;
    type Records = Rc<RefCell<Option<Vec<u8>>>>;

    struct MemoryHandle {
        image: Image,
        sector: u64,
    }

    impl TargetHandle for MemoryHandle {
        fn path(&self) -> &Path {
            Path::new("<memory>")
        }

        fn kind(&self) -> TargetKind {
            TargetKind::File
        }

        fn len(&self) -> u64 {
            self.image.borrow().len() as u64
        }

        fn sector_size(&self) -> Option<u64> {
            Some(self.sector)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            let image = self.image.borrow();
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= image.len() {
                return Ok(0);
            }
            let len = buf.len().min(image.len() - start);
            buf[..len].copy_from_slice(&image[start..start + len]);
            Ok(len)
        }

        fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
            let mut image = self.image.borrow_mut();
            let start = usize::try_from(offset)
                .map_err(|_| PlatformError::io("<memory>", std::io::Error::other("偏移过大")))?;
            if start + buf.len() > image.len() {
                return Err(PlatformError::io(
                    "<memory>",
                    std::io::Error::other("越界写入"),
                ));
            }
            image[start..start + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            Ok(())
        }

        fn lock_exclusive(&mut self) -> PlatformResult<()> {
            Ok(())
        }
    }

    struct MemoryStore {
        active: Records,
    }

    impl RecordStore for MemoryStore {
        fn location(&self) -> &Path {
            Path::new("<records>")
        }

        fn load(&self) -> PlatformResult<Option<Vec<u8>>> {
            Ok(self.active.borrow().clone())
        }

        fn store(&mut self, bytes: &[u8]) -> PlatformResult<()> {
            *self.active.borrow_mut() = Some(bytes.to_vec());
            Ok(())
        }

        fn clear(&mut self) -> PlatformResult<()> {
            *self.active.borrow_mut() = None;
            Ok(())
        }

        fn entries(&self) -> PlatformResult<Vec<RecordEntry>> {
            Ok(Vec::new())
        }

        fn quarantine(&mut self) -> PlatformResult<String> {
            Err(PlatformError::not_supported("测试不归档"))
        }
    }

    struct FakeHost {
        image: Image,
        records: Records,
        tools: RefCell<VecDeque<(&'static str, bool)>>,
    }

    impl FakeHost {
        fn new(len: usize) -> Self {
            Self {
                image: Rc::new(RefCell::new(vec![0u8; len])),
                records: Rc::new(RefCell::new(None)),
                tools: RefCell::new(VecDeque::new()),
            }
        }
    }

    impl Platform for FakeHost {
        fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
            Ok(Box::new(MemoryHandle {
                image: Rc::clone(&self.image),
                sector: SECTOR,
            }))
        }

        fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
            self.open_target(_path)
        }

        fn fingerprint(&self, _path: &Path) -> PlatformResult<Fingerprint> {
            Ok(Fingerprint::strong([3; 32]))
        }

        fn partitions(&self, _path: &Path) -> PlatformResult<Vec<Partition>> {
            Ok(Vec::new())
        }

        fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
            Ok(Occupancy {
                mounts: Vec::<MountEntry>::new(),
                swaps: Vec::<SwapEntry>::new(),
                holders: Vec::new(),
                array_member: false,
                loops: Vec::new(),
            })
        }

        fn reread(&self, _path: &Path) -> PlatformResult<()> {
            Ok(())
        }

        fn update_table(&self, _path: &Path) -> PlatformResult<()> {
            Ok(())
        }

        fn btrfs_fsid(&self, _path: &Path) -> PlatformResult<[u8; 16]> {
            Err(PlatformError::not_supported("测试不读 FSID"))
        }

        fn loop_attach(
            &self,
            _backing: &Path,
            _offset: u64,
            _size: u64,
        ) -> PlatformResult<PathBuf> {
            Err(PlatformError::not_supported("测试没有 loop"))
        }

        fn loop_detach(&self, _device: &Path) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试没有 loop"))
        }

        fn mount(
            &self,
            _source: &Path,
            _mount_point: &Path,
            _fstype: Option<&str>,
            _options: &[&str],
        ) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试不挂载"))
        }

        fn umount(&self, _mount_point: &Path) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试不卸载"))
        }

        fn require(&self, _program: Program) -> PlatformResult<PathBuf> {
            Ok(PathBuf::from("/usr/bin/program"))
        }

        fn run(&self, _program: Program, _args: &[&str]) -> PlatformResult<Output> {
            match self.tools.borrow_mut().pop_front() {
                Some((_, success)) if success => Ok(Output::default()),
                Some((why, _)) => Err(PlatformError::tools("program", why)),
                None => Err(PlatformError::not_supported("测试不执行程序")),
            }
        }

        fn records(
            &self,
            _target: &Path,
            _identity: Fingerprint,
        ) -> PlatformResult<Box<dyn RecordStore>> {
            Ok(Box::new(MemoryStore {
                active: Rc::clone(&self.records),
            }))
        }
    }

    fn confirmed_new(host: &FakeHost) -> (ConfirmedPlan, Snapshot) {
        let request = Request::new(
            "/tmp/image",
            Command::New {
                table: TableFormat::Gpt,
                overwrite: true,
            },
        );
        let snapshot = observe(&request, host);
        let decision = decide(&request, &snapshot);
        let plan = match decision {
            Decision::Plan { plan } => plan,
            Decision::Refuse { refusal } => panic!("新建计划被拒绝：{refusal:?}"),
        };
        let plan: Plan = plan;
        let confirmed = confirm(plan, true, &snapshot.identity).expect("确认应成立");
        (confirmed, snapshot)
    }

    #[test]
    fn write_before_record_leaves_target_untouched() {
        let host = FakeHost::new(2 * 1024 * 1024);
        let (confirmed, _snapshot) = confirmed_new(&host);
        let before = host.image.borrow().clone();
        let outcome = run(
            &confirmed,
            &host,
            Path::new("/tmp/image"),
            1,
            Faults {
                before_write: true,
                ..Faults::NONE
            },
        );
        assert_eq!(outcome.state(), ExecutionState::Unwritten);
        assert_eq!(*host.image.borrow(), before);
        assert!(host.records.borrow().is_none());
    }

    #[test]
    fn write_mid_step_leaves_record_and_needs_recovery() {
        let host = FakeHost::new(2 * 1024 * 1024);
        let (confirmed, _snapshot) = confirmed_new(&host);
        let outcome = run(
            &confirmed,
            &host,
            Path::new("/tmp/image"),
            1,
            Faults {
                mid_write: true,
                ..Faults::NONE
            },
        );
        assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
        assert!(host.records.borrow().is_some());
    }

    #[test]
    fn write_after_completion_without_commit_needs_recovery() {
        let host = FakeHost::new(2 * 1024 * 1024);
        let (confirmed, _snapshot) = confirmed_new(&host);
        let outcome = run(
            &confirmed,
            &host,
            Path::new("/tmp/image"),
            1,
            Faults {
                after_write: true,
                ..Faults::NONE
            },
        );
        assert_eq!(outcome.state(), ExecutionState::NeedsRecovery);
        assert!(host.records.borrow().is_some());
    }

    #[test]
    fn successful_new_writes_table_and_clears_record() {
        let host = FakeHost::new(2 * 1024 * 1024);
        let (confirmed, _snapshot) = confirmed_new(&host);
        let outcome = run(&confirmed, &host, Path::new("/tmp/image"), 1, Faults::NONE);
        assert!(outcome.succeeded(), "{outcome:?}");
        assert!(host.records.borrow().is_none());
        let image = host.image.borrow();
        assert_eq!(&image[510..512], &[0x55, 0xAA]);
        assert_ne!(&image[512..516], &[0, 0, 0, 0]);
    }
}
