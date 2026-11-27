//! 搬移/复制的记录现场。
//!
//! 它与 [`super::RecordSession`] 同属 `transaction` 模块（这里是一个子模块），
//! 因此能使用该模块私有的铸造权 [`Mint`]。把会话放在铸造权所在模块之内，
//! `Mint` 的构造函数就可以保持私有：`execute` 下其他模块（写通道、令牌）
//! 拿不到 `Mint`，也就无法伪造令牌。

use std::path::PathBuf;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{self, Frame, FrameKind, RecordBody};
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
        let body = RecordBody {
            target: identity,
            geometry_generation: 0,
            plan_digest,
        };

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
    pub(crate) fn commit(&mut self) -> Result<(), (ExecError, bool)> {
        self.append(Frame {
            seq: u32::try_from(self.frames.len()).unwrap_or(u32::MAX),
            kind: FrameKind::Committed,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: Vec::new(),
        })?;
        self.platform
            .remove_record(&self.file)
            .map_err(|error| (ExecError::from(error), false))?;
        Ok(())
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
