//! 搬移/复制的记录现场。
//!
//! 它与 [`super::RecordSession`] 同属 `transaction` 模块（这里是一个子模块），
//! 因此能使用该模块私有的铸造权 [`Mint`]。把会话放在铸造权所在模块之内，
//! `Mint` 的构造函数就可以保持私有：`execute` 下其他模块（写通道、令牌）
//! 拿不到 `Mint`，也就无法伪造令牌。

use std::path::PathBuf;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{self, Frame, FrameKind, RecordBody, geometry_generation};
use diskedit_core::region::Region;

use super::{LOCK_FILE, Mint, preimage};
use crate::app::execute::token::SessionId;
use crate::app::execute::{ExecError, Recorded};
use crate::app::observe::records::{RECORD_FILE, hex16};
use crate::platform::{Platform, StateLock};

/// 可续跑游标：已写块数与已写前缀摘要。
type ResumePoint = (u64, [u8; 16]);

/// 执行失败与"目标是否可能已被写过"的成对结果。
type ExecFailure = (ExecError, bool);

/// 提交结果。
///
/// 提交由两步组成：先写终态帧，再删除记录。两步各自会失败，失败后的盘面状态
/// 不同，因此结果不能折叠成一个布尔；走到提交时目标字节必然已经写过（本次或
/// 上一次运行），任何一步失败都要按"已写、待恢复"报告，不能报"未写"。
pub(crate) enum Commit {
    /// 终态帧已落盘且记录已清理。
    Clean,
    /// 终态帧已落盘，记录未清理：事务已提交，只剩清理。
    RecordLeft(ExecError),
    /// 终态帧未落盘：事务未收尾，盘上仍是不带终态帧的记录。
    Unfinished(ExecError),
}

/// 一次搬移/复制的记录现场。
pub(crate) struct Session<'a> {
    platform: &'a dyn Platform,
    file: PathBuf,
    _lock: Box<dyn StateLock>,
    body: RecordBody,
    frames: Vec<Frame>,
    mint: Mint,
    session: SessionId,
    minted: Vec<u64>,
    table_registered: bool,
}

impl<'a> Session<'a> {
    /// 取锁、读出可续跑进度；没有记录时先落一条意图帧。
    pub(crate) fn prepare(
        platform: &'a dyn Platform,
        identity: Fingerprint,
        plan_digest: [u8; 32],
        spec: &[u8],
    ) -> Result<(Self, Option<ResumePoint>), ExecFailure> {
        let root = platform.state_root().ok_or_else(|| {
            (
                ExecError::Record("the host has no state directory".to_owned()),
                false,
            )
        })?;
        let directory = root.join("images").join(hex16(identity));
        platform
            .create_directory_all(&directory)
            .map_err(|error| (ExecError::from(error), false))?;
        let lock = platform
            .lock_state(&directory.join(LOCK_FILE))
            .map_err(|error| (ExecError::from(error), false))?;
        let file = directory.join(RECORD_FILE);
        let total_bytes = spec_total_bytes(spec).ok_or_else(|| {
            (
                ExecError::Record("the relocation geometry carries no target size".to_owned()),
                false,
            )
        })?;

        let existing = crate::app::observe::records::read_bytes(platform, &file)
            .map_err(|error| (ExecError::from(error), false))?;
        if let Some(bytes) = existing.filter(|bytes| !bytes.is_empty()) {
            let decoded = record::decode(&bytes).map_err(|error| {
                (
                    ExecError::Record(format!("the existing record is unreadable: {error}")),
                    false,
                )
            })?;
            if !decoded.body.target.matches(identity) {
                return Err((
                    ExecError::Record("the record targets a different device".to_owned()),
                    false,
                ));
            }
            if decoded.body.plan_digest != plan_digest {
                return Err((
                    ExecError::Record("the existing record belongs to a different plan".to_owned()),
                    false,
                ));
            }
            let Some(Frame {
                kind: FrameKind::Intent,
                intent,
                ..
            }) = decoded.frames.first()
            else {
                return Err((
                    ExecError::Record("the record does not begin with an intent".to_owned()),
                    false,
                ));
            };
            if intent.as_slice() != spec {
                return Err((
                    ExecError::Record(
                        "the record's relocation geometry does not match this request".to_owned(),
                    ),
                    false,
                ));
            }
            let resume = decoded.frames.iter().rev().find_map(|frame| {
                if frame.kind == FrameKind::Progress && frame.intent.len() == 24 {
                    Some(progress_of(&frame.intent))
                } else {
                    None
                }
            });
            let table_registered = decoded
                .frames
                .iter()
                .any(|frame| frame.kind == FrameKind::Preimage);
            // 续跑保留盘上记录的载荷前缀：几何代次以记录写下时为准。
            let body = decoded.body;
            let frames = decoded.frames;
            return Ok((
                Self {
                    platform,
                    file,
                    _lock: lock,
                    body,
                    frames,
                    mint: Mint::new(),
                    session: SessionId::next(),
                    minted: Vec::new(),
                    table_registered,
                },
                resume,
            ));
        }

        let body = RecordBody {
            target: identity,
            legacy_generation: geometry_generation(total_bytes),
            plan_digest,
        };
        let intent = Frame {
            seq: 0,
            kind: FrameKind::Intent,
            completed: false,
            irreversible: false,
            intent: spec.to_vec(),
            regions: Vec::new(),
        };
        let frames = vec![intent];
        let encoded = record::encode(&body, &frames).map_err(|error| {
            (
                ExecError::Record(format!("the record cannot be encoded: {error}")),
                false,
            )
        })?;
        platform
            .write_record(&file, &encoded)
            .map_err(|error| (ExecError::from(error), false))?;
        Ok((
            Self {
                platform,
                file,
                _lock: lock,
                body,
                frames,
                mint: Mint::new(),
                session: SessionId::next(),
                minted: Vec::new(),
                table_registered: false,
            },
            None,
        ))
    }

    pub(crate) fn token(
        &mut self,
        step: u64,
        regions: &[Region],
    ) -> Result<Recorded, (ExecError, bool)> {
        if self.minted.contains(&step) {
            return Err((
                ExecError::Record(format!("step {step} already spent its write token")),
                false,
            ));
        }
        self.minted.push(step);
        Ok(Recorded::mint(&self.mint, self.session, step, regions))
    }

    pub(crate) const fn id(&self) -> SessionId {
        self.session
    }

    pub(crate) fn table_registered(&self) -> bool {
        self.table_registered
    }

    /// 追加一条进度帧：块游标与已写前缀摘要。
    pub(crate) fn checkpoint(
        &mut self,
        cursor: u64,
        digest: [u8; 16],
    ) -> Result<(), (ExecError, bool)> {
        let mut intent = Vec::with_capacity(24);
        intent.extend_from_slice(&cursor.to_le_bytes());
        intent.extend_from_slice(&digest);
        self.append(Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Progress,
            completed: true,
            irreversible: true,
            intent,
            regions: Vec::new(),
        })
    }

    /// 登记表区原像与写后字节；已有原像帧时复用。
    pub(crate) fn register_table(
        &mut self,
        segments: &[(Region, Vec<u8>, Vec<u8>)],
    ) -> Result<(), (ExecError, bool)> {
        if self.table_registered {
            return Ok(());
        }
        let frame = Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Preimage,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: segments
                .iter()
                .map(|(region, before, after)| preimage(*region, before, after))
                .collect(),
        };
        self.append(frame)?;
        self.table_registered = true;
        Ok(())
    }

    /// 追加提交帧、落盘并删除记录。
    ///
    /// 返回三态而不是布尔：终态帧是否落盘、记录是否清理是两件事，调用方据此
    /// 决定报告哪一种"已写"。
    pub(crate) fn commit(&mut self) -> Commit {
        let frame = Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Committed,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: Vec::new(),
        };
        if let Err((error, _)) = self.append(frame) {
            return Commit::Unfinished(error);
        }
        match self.platform.remove_record(&self.file) {
            Ok(()) => Commit::Clean,
            Err(error) => Commit::RecordLeft(ExecError::from(error)),
        }
    }

    fn append(&mut self, frame: Frame) -> Result<(), (ExecError, bool)> {
        let mut frames = self.frames.clone();
        frames.push(frame);
        let encoded = record::encode(&self.body, &frames).map_err(|error| {
            (
                ExecError::Record(format!("the record cannot be encoded: {error}")),
                false,
            )
        })?;
        self.platform
            .write_record(&self.file, &encoded)
            .map_err(|error| (ExecError::from(error), false))?;
        self.frames = frames;
        Ok(())
    }
}

fn progress_of(intent: &[u8]) -> (u64, [u8; 16]) {
    let mut cursor = [0u8; 8];
    cursor.copy_from_slice(&intent[..8]);
    let mut digest = [0u8; 16];
    digest.copy_from_slice(&intent[8..24]);
    (u64::from_le_bytes(cursor), digest)
}

/// 搬移意图帧几何快照末尾的目标总字节数。
pub(super) fn spec_total_bytes(spec: &[u8]) -> Option<u64> {
    let at = spec.len().checked_sub(8)?;
    let tail: [u8; 8] = spec.get(at..)?.try_into().ok()?;
    Some(u64::from_le_bytes(tail))
}
