//! 事务：先取锁、再核对、登记原像之后才铸造写令牌，提交后才清理记录。
//!
//! 记录目录按目标身份派生，独占锁与记录文件同目录。首帧声明意图，第二帧登记
//! 本步将覆盖区间的原像；目标写完后追加提交帧并删除记录。中断时记录留在原处，
//! 由恢复流程处理。

use std::path::PathBuf;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{
    self, Frame, FrameKind, Preimage, RecordBody, frame::PREIMAGE_DIGEST_BYTES, geometry_generation,
};
use diskedit_core::region::Region;

use super::digest::Hasher;
use super::token::SessionId;
use super::{ExecError, Recorded};
use crate::app::observe::records::{RECORD_FILE, hex16};
use crate::platform::{Platform, StateLock};

mod relocate_session;

pub(crate) use relocate_session::{Commit, Session as RelocateSession};

/// 记录目录里的锁文件名。
pub const LOCK_FILE: &str = "record.lock";

/// 写入令牌的铸造权。
///
/// 元组字段私有，模块外无法构造；持有者才能铸造 [`Recorded`]。
///
/// [`Recorded`]: super::token::Recorded
/// 铸造写令牌的凭据。
///
/// 构造函数只在 `transaction` 模块内可见（`relocate_session` 作为子模块也能用），
/// 因此模块之外的代码，哪怕是 crate 内其他模块，也拿不到 `Mint`，无法伪造令牌。
pub struct Mint(());

impl Mint {
    const fn new() -> Self {
        Self(())
    }

    /// 令牌用它留下“由铸币者签发”的痕迹；真正防伪靠构造函数私有。
    pub(crate) const fn brand(&self) -> std::marker::PhantomData<Self> {
        std::marker::PhantomData
    }

    /// 仅供测试模块构造；生产代码没有别的入口。
    #[cfg(test)]
    pub(crate) const fn for_test() -> Self {
        Self(())
    }
}

/// 一次写目标字节的事务现场。
pub struct RecordSession<'a> {
    platform: &'a dyn Platform,
    file: PathBuf,
    _lock: Box<dyn StateLock>,
    mint: Mint,
    /// 本会话的标识；令牌与写通道据此拒绝别的会话。
    session: SessionId,
    /// 已铸造过令牌的步骤号；同一步骤只发一枚，重放没有第二枚可用。
    minted: Vec<u64>,
    /// 本次事务绑定的目标身份，同时用于记录载荷与目录定位。
    bound: Fingerprint,
    /// 本次事务看到的目标总字节数展开出的几何代次。
    geometry: u64,
    /// 已有的未终结记录所绑定的摘要。
    pending: Option<[u8; 32]>,
    /// 复用的记录已有这么多帧。
    sequence: u32,
    registered: bool,
}

impl<'a> RecordSession<'a> {
    /// 取目标记录目录的独占锁，并读出仍在进行中的记录（若有）。
    pub fn prepare(
        platform: &'a dyn Platform,
        identity: Fingerprint,
        total_bytes: u64,
    ) -> Result<Self, ExecError> {
        let root = platform
            .state_root()
            .ok_or_else(|| ExecError::Record("the host has no state directory".to_owned()))?;
        let directory = root.join("images").join(hex16(identity));
        platform
            .create_directory_all(&directory)
            .map_err(ExecError::from)?;
        let lock = platform
            .lock_state(&directory.join(LOCK_FILE))
            .map_err(ExecError::from)?;
        let file = directory.join(RECORD_FILE);

        let mut pending = None;
        let mut sequence = 0;
        if let Some(bytes) =
            crate::app::observe::records::read_bytes(platform, &file).map_err(ExecError::from)?
            && !bytes.is_empty()
        {
            let decoded =
                record::decode(&bytes).map_err(|error| ExecError::Record(error.to_string()))?;
            // 记录目录按身份派生，但记录内容仍要自证归属：末帧未终结只说明"没做完"，
            // 不说明它是本目标的记录。指向别的载体时拒绝，不能把它当作本目标的原像。
            if !decoded.body.target.matches(identity) {
                return Err(ExecError::Record(
                    "the record targets a different device".to_owned(),
                ));
            }
            let last_pending = decoded
                .frames
                .last()
                .map(|frame| !matches!(frame.kind, FrameKind::Committed | FrameKind::Aborted))
                .unwrap_or(true);
            if last_pending {
                pending = Some(decoded.body.plan_digest);
                sequence = u32::try_from(decoded.frames.len())
                    .map_err(|_| ExecError::Record("record has too many frames".to_owned()))?;
            }
        }
        Ok(Self {
            platform,
            file,
            _lock: lock,
            mint: Mint::new(),
            session: SessionId::next(),
            minted: Vec::new(),
            bound: identity,
            geometry: geometry_generation(total_bytes),
            pending,
            sequence,
            registered: false,
        })
    }

    /// 已有一条针对当前计划的未终结记录：本次从中断处继续。
    pub fn resumed(&self) -> bool {
        self.pending.is_some()
    }

    /// 登记原像与写后字节；已有同计划记录时复用它，不重写原像。
    pub fn register(
        &mut self,
        plan_digest: [u8; 32],
        segments: &[(Region, Vec<u8>, Vec<u8>)],
    ) -> Result<(), ExecError> {
        if let Some(pending) = self.pending {
            if pending != plan_digest {
                return Err(ExecError::Record(
                    "the existing record belongs to a different plan".to_owned(),
                ));
            }
            self.registered = true;
            return Ok(());
        }
        let body = RecordBody {
            target: self.identity(),
            legacy_generation: self.geometry,
            plan_digest,
        };
        let frames = [
            Frame {
                seq: 0,
                kind: FrameKind::Intent,
                completed: false,
                irreversible: false,
                intent: b"write-table".to_vec(),
                regions: Vec::new(),
            },
            Frame {
                seq: 1,
                kind: FrameKind::Preimage,
                completed: true,
                irreversible: false,
                intent: Vec::new(),
                regions: segments
                    .iter()
                    .map(|(region, before, after)| preimage(*region, before, after))
                    .collect(),
            },
        ];
        let encoded =
            record::encode(&body, &frames).map_err(|error| ExecError::Record(error.to_string()))?;
        self.platform
            .write_record(&self.file, &encoded)
            .map_err(ExecError::from)?;
        self.sequence = 2;
        // 记录此刻已经落盘：后续步骤再登记原像时，当前字节已是本事务写过的样子，
        // 必须复用这条记录，不能拿它覆盖掉更早的原像。
        self.pending = Some(plan_digest);
        self.registered = true;
        Ok(())
    }

    /// 声明一步不可回滚的改写意图；已有同计划的未终结记录时复用它。
    ///
    /// 这类步骤（例如调用外部建卷程序）没有原像可回滚；意图帧落盘并同步目录
    /// 之后才允许动目标字节。中断时记录留在原处，由恢复流程报告待恢复。
    pub fn begin_irreversible(
        &mut self,
        plan_digest: [u8; 32],
        intent: &[u8],
    ) -> Result<(), ExecError> {
        if let Some(pending) = self.pending {
            if pending != plan_digest {
                return Err(ExecError::Record(
                    "the existing record belongs to a different plan".to_owned(),
                ));
            }
            self.registered = true;
            return Ok(());
        }
        let body = RecordBody {
            target: self.identity(),
            legacy_generation: self.geometry,
            plan_digest,
        };
        let frames = [Frame {
            seq: 0,
            kind: FrameKind::Intent,
            completed: false,
            irreversible: true,
            intent: intent.to_vec(),
            regions: Vec::new(),
        }];
        let encoded =
            record::encode(&body, &frames).map_err(|error| ExecError::Record(error.to_string()))?;
        self.platform
            .write_record(&self.file, &encoded)
            .map_err(ExecError::from)?;
        self.sequence = 1;
        self.pending = Some(plan_digest);
        self.registered = true;
        Ok(())
    }

    /// 外部不可逆动作成功后追加完成帧。
    ///
    /// `begin_irreversible` 落下的意图帧是 `completed:false`：中断时恢复器无法
    /// 判断外部动作是否已发生。驱动在外部动作成功后调用本方法把完成标记落盘，
    /// 恢复器据此把"已完成的不可逆步骤"与"未完成的不可逆步骤"分开。
    pub fn complete_irreversible(&mut self) -> Result<(), ExecError> {
        if !self.registered {
            return Err(ExecError::Record(
                "no irreversible step was declared".to_owned(),
            ));
        }
        self.rewrite_append(FrameKind::Progress, true, true, Vec::new())
    }

    /// 读回记录、追加一帧并整条重编码落盘。
    fn rewrite_append(
        &mut self,
        kind: FrameKind,
        completed: bool,
        irreversible: bool,
        intent: Vec<u8>,
    ) -> Result<(), ExecError> {
        let Some(existing) = crate::app::observe::records::read_bytes(self.platform, &self.file)
            .map_err(ExecError::from)?
        else {
            return Err(ExecError::Record(
                "the record vanished before it was completed".to_owned(),
            ));
        };
        let mut decoded =
            record::decode(&existing).map_err(|error| ExecError::Record(error.to_string()))?;
        let seq = u32::try_from(decoded.frames.len())
            .map_err(|_| ExecError::Record("record has too many frames".to_owned()))?;
        decoded.frames.push(Frame {
            seq,
            kind,
            completed,
            irreversible,
            intent,
            regions: Vec::new(),
        });
        let encoded = record::encode(&decoded.body, &decoded.frames)
            .map_err(|error| ExecError::Record(error.to_string()))?;
        self.platform
            .write_record(&self.file, &encoded)
            .map_err(ExecError::from)?;
        // 下一条追加帧（例如提交帧）接着用这个序号，不能与刚追加的重复。
        self.sequence = seq + 1;
        Ok(())
    }

    /// 本步原像已落盘，可以写目标字节。
    ///
    /// 令牌按值交出并绑定 `(会话, 步骤号, 区间摘要)`；同一步骤再次索取会失败，
    /// 因此没有第二枚令牌可用于重放。
    pub fn token(&mut self, step: u64, regions: &[Region]) -> Result<Recorded, ExecError> {
        if self.minted.contains(&step) {
            return Err(ExecError::Record(format!(
                "step {step} already spent its write token"
            )));
        }
        self.minted.push(step);
        Ok(Recorded::mint(&self.mint, self.session, step, regions))
    }

    /// 本会话的标识。
    pub const fn id(&self) -> SessionId {
        self.session
    }

    /// 追加提交帧、落盘并删除记录。
    ///
    /// 终态必须整条重写：只追加一帧而不更新头部时，恢复器按头声明长度截取载荷，
    /// 看不到这条终态，会把已提交的写入当成待回滚。重写后即使删除记录前崩溃，
    /// 盘上也已是完整的已提交记录。
    pub fn commit(&mut self) -> Result<(), ExecError> {
        if !self.registered {
            return Err(ExecError::Record("no preimage was registered".to_owned()));
        }
        self.rewrite_append(FrameKind::Committed, true, false, Vec::new())?;
        self.platform
            .remove_record(&self.file)
            .map_err(ExecError::from)?;
        Ok(())
    }

    fn identity(&self) -> Fingerprint {
        self.bound
    }
}

fn preimage(region: Region, before: &[u8], after: &[u8]) -> Preimage {
    let mut hasher = Hasher::new();
    hasher.update(before);
    let full = hasher.finish();
    let mut digest = [0u8; PREIMAGE_DIGEST_BYTES];
    digest.copy_from_slice(&full[..PREIMAGE_DIGEST_BYTES]);
    Preimage {
        offset: region.start(),
        len: region.len(),
        digest,
        data: before.to_vec(),
        after: after.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use diskedit_core::record::{RecoveryExpectation, RecoveryPlan, recover};

    use super::*;
    use crate::platform::{
        LoopBinding, Occupancy, PlatformError, PlatformResult, RecordEntry, StateLock,
        TargetHandle, TargetKind, ToolRun,
    };

    const IDENTITY: Fingerprint = Fingerprint::strong([0x42; 32]);
    /// 测试里记录会话看到的目标总字节数。
    const TOTAL_BYTES: u64 = 1 << 20;

    /// 记录目录里的记录文件位置，与 [`RecordSession::prepare`] 拼出的路径一致。
    fn record_path() -> PathBuf {
        PathBuf::from("/state")
            .join("images")
            .join(hex16(IDENTITY))
            .join(RECORD_FILE)
    }

    struct Lock;

    impl StateLock for Lock {}

    /// 只实现记录位置读写的宿主；其余端口显式失败，不假装成功。
    #[derive(Default)]
    struct Memory {
        files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
        /// 置位后下一次记录写入失败，随后自动复位。
        fail_next_write: AtomicBool,
        /// 置位后下一次记录删除失败，随后自动复位。
        fail_next_remove: AtomicBool,
    }

    impl Memory {
        fn bytes(&self) -> Vec<u8> {
            self.files
                .lock()
                .unwrap()
                .get(&record_path())
                .cloned()
                .expect("记录必须留在记录位置")
        }

        fn has_record(&self) -> bool {
            self.files.lock().unwrap().contains_key(&record_path())
        }
    }

    impl Platform for Memory {
        fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
            Err(PlatformError::not_supported("no target handle"))
        }
        fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
            Err(PlatformError::not_supported("no target handle"))
        }
        fn path_kind(&self, _path: &Path) -> TargetKind {
            TargetKind::Other
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
            if self.fail_next_write.swap(false, Ordering::SeqCst) {
                return Err(PlatformError::parse("injected record write failure"));
            }
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }
        fn remove_record(&self, path: &Path) -> PlatformResult<()> {
            if self.fail_next_remove.swap(false, Ordering::SeqCst) {
                return Err(PlatformError::parse("injected record removal failure"));
            }
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

    fn segments(
        region: Region,
        before: Vec<u8>,
        after: Vec<u8>,
    ) -> Vec<(Region, Vec<u8>, Vec<u8>)> {
        vec![(region, before, after)]
    }

    #[test]
    fn a_second_registration_keeps_the_earlier_preimage() {
        // 同一计划的第二个写步骤再次登记原像时，当前字节已经是第一次写后的样子。
        // 记录里的原像必须仍是第一次写之前的字节，否则回滚只能退到第一次写之后。
        let memory = Memory::default();
        let plan = [0x11u8; 32];
        let region = Region::sized(0, 512).expect("区间");
        let original = vec![0xAAu8; 512];
        let written = vec![0xBBu8; 512];
        let mut session =
            RecordSession::prepare(&memory, IDENTITY, TOTAL_BYTES).expect("准备应成立");
        session
            .register(plan, &segments(region, original.clone(), written.clone()))
            .expect("首次登记应成立");
        let first = memory.bytes();

        session
            .register(plan, &segments(region, written, vec![0xCCu8; 512]))
            .expect("同计划续跑登记应成立");
        assert_eq!(
            memory.bytes(),
            first,
            "同计划的第二次登记不得改写已经落盘的原像"
        );

        let decoded = record::decode(&first).expect("记录应可解码");
        let stored = decoded
            .frames
            .iter()
            .flat_map(|frame| &frame.regions)
            .find(|preimage| preimage.offset == 0)
            .expect("记录里应有覆盖该区间的原像");
        assert_eq!(stored.data, original, "记录里必须是第一次写之前的字节");
    }

    #[test]
    fn a_registration_for_another_plan_is_refused() {
        let memory = Memory::default();
        let region = Region::sized(0, 512).expect("区间");
        let mut session =
            RecordSession::prepare(&memory, IDENTITY, TOTAL_BYTES).expect("准备应成立");
        session
            .register(
                [0x11u8; 32],
                &segments(region, vec![0xAA; 512], vec![0xBB; 512]),
            )
            .expect("首次登记应成立");
        let error = session
            .register(
                [0x22u8; 32],
                &segments(region, vec![0xBB; 512], vec![0xCC; 512]),
            )
            .expect_err("另一份计划不得接管这条记录");
        assert!(
            error.to_string().contains("different plan"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn an_irreversible_step_is_marked_before_it_runs_and_resumes_by_the_marker() {
        let memory = Memory::default();
        let plan = [0x33u8; 32];
        let mut first = RecordSession::prepare(&memory, IDENTITY, TOTAL_BYTES).expect("准备应成立");
        assert!(!first.resumed(), "还没有记录时不是续跑");
        first
            .begin_irreversible(plan, b"make-filesystem")
            .expect("不可逆意图应落盘");

        // 外部动作还没成功就中断：盘上只有一条未完成的不可逆意图。
        let bytes = memory.bytes();
        let decoded = record::decode(&bytes).expect("记录应可解码");
        assert_eq!(decoded.frames.len(), 1, "进入不可逆前只落一条意图");
        assert!(decoded.frames[0].irreversible, "意图必须标为不可逆");
        assert!(!decoded.frames[0].completed, "外部动作尚未成功");
        assert_eq!(
            recover(&bytes, RecoveryExpectation::new(IDENTITY, TOTAL_BYTES)),
            RecoveryPlan::RollBack {
                frames: decoded.frames.clone()
            },
            "未完成的不可逆记录不是已提交"
        );
        drop(first);

        // 续跑：同一计划被接受，外部动作成功后补上完成标记。
        let mut second =
            RecordSession::prepare(&memory, IDENTITY, TOTAL_BYTES).expect("准备应成立");
        assert!(second.resumed(), "续跑必须从记录认出未完成的不可逆阶段");
        second
            .begin_irreversible(plan, b"make-filesystem")
            .expect("同计划续跑应被接受");
        second.complete_irreversible().expect("完成标记应落盘");

        let decoded = record::decode(&memory.bytes()).expect("记录应可解码");
        assert_eq!(decoded.frames.len(), 2, "完成标记是额外一帧");
        assert!(
            decoded.frames[1].completed && decoded.frames[1].irreversible,
            "外部成功后必须留下已完成的不可逆标记：{:?}",
            decoded.frames
        );
    }

    #[test]
    fn a_commit_whose_terminal_frame_is_lost_is_unfinished_not_clean() {
        let memory = Memory::default();
        let (mut session, _) =
            RelocateSession::prepare(&memory, IDENTITY, [0x44u8; 32], b"diskedit-relocate-v1")
                .expect("准备应成立");
        memory.fail_next_write.store(true, Ordering::SeqCst);
        assert!(
            matches!(session.commit(), Commit::Unfinished(_)),
            "终态帧没落盘时不能报成已提交"
        );
        let decoded = record::decode(&memory.bytes()).expect("记录应可解码");
        assert!(
            decoded
                .frames
                .last()
                .is_some_and(|frame| frame.kind != FrameKind::Committed),
            "盘上不应出现终态帧"
        );
    }

    #[test]
    fn a_commit_that_cannot_clear_the_record_is_record_left() {
        let memory = Memory::default();
        let (mut session, _) =
            RelocateSession::prepare(&memory, IDENTITY, [0x44u8; 32], b"diskedit-relocate-v1")
                .expect("准备应成立");
        memory.fail_next_remove.store(true, Ordering::SeqCst);
        assert!(
            matches!(session.commit(), Commit::RecordLeft(_)),
            "终态帧已落盘、记录未清理是第三种结果"
        );
        let total = super::relocate_session::spec_total_bytes(b"diskedit-relocate-v1")
            .expect("规格携带目标大小");
        assert_eq!(
            recover(&memory.bytes(), RecoveryExpectation::new(IDENTITY, total)),
            RecoveryPlan::Committed,
            "记录未清理时盘上必须已经是完整的已提交记录"
        );
    }

    #[test]
    fn a_commit_that_clears_the_record_is_clean() {
        let memory = Memory::default();
        let (mut session, _) =
            RelocateSession::prepare(&memory, IDENTITY, [0x44u8; 32], b"diskedit-relocate-v1")
                .expect("准备应成立");
        assert!(matches!(session.commit(), Commit::Clean));
        assert!(!memory.has_record(), "提交成功后记录应被清理");
    }

    #[test]
    fn a_checkpointed_cursor_survives_a_new_session() {
        let memory = Memory::default();
        let plan = [0x55u8; 32];
        let spec = b"diskedit-relocate-v1";
        let (mut first, resume) =
            RelocateSession::prepare(&memory, IDENTITY, plan, spec).expect("准备应成立");
        assert_eq!(resume, None, "没有进度时从头开始");
        first.checkpoint(3, [0xAB; 16]).expect("进度帧应落盘");

        // 会话随进程消失，只剩下记录字节。
        drop(first);
        let (_second, resume) =
            RelocateSession::prepare(&memory, IDENTITY, plan, spec).expect("准备应成立");
        assert_eq!(
            resume,
            Some((3, [0xAB; 16])),
            "块游标与已写前缀摘要必须随记录落盘，而不是只留在会话里"
        );
    }

    #[test]
    fn mint_grants_recorded_tokens() {
        let mint = Mint::new();
        let region = Region::sized(0, 512).expect("区间");
        let token = Recorded::mint(&mint, SessionId::next(), 7, &[region]);
        assert_eq!(token.step(), 7);
        assert_eq!(
            token.region_digest(),
            super::super::token::RegionDigest::of(&[region]).finish()
        );
    }

    #[test]
    fn preimage_digest_covers_the_bytes() {
        let region = Region::sized(512, 4).expect("区间");
        let left = preimage(region, &[1, 2, 3, 4], &[9, 9, 9, 9]);
        let right = preimage(region, &[1, 2, 3, 5], &[9, 9, 9, 9]);
        assert_ne!(left.digest, right.digest);
        assert_eq!(left.offset, 512);
        assert_eq!(left.len, 4);
        assert_eq!(left.after, vec![9, 9, 9, 9]);
    }
}
