//! 事务：先取锁、再核对、登记原像之后才铸造写令牌，提交后才清理记录。
//!
//! 记录目录按目标身份派生，独占锁与记录文件同目录。首帧声明意图，第二帧登记
//! 本步将覆盖区间的原像；目标写完后追加提交帧并删除记录。中断时记录留在原处，
//! 由恢复流程处理。

use std::path::PathBuf;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{
    self, Frame, FrameKind, Preimage, RecordBody, frame::PREIMAGE_DIGEST_BYTES,
};
use diskedit_core::region::Region;

use super::digest::Hasher;
use super::token::SessionId;
use super::{ExecError, Recorded};
use crate::app::observe::records::{RECORD_FILE, hex16};
use crate::platform::{Platform, RecordEntry, StateLock};

/// 记录目录里的锁文件名。
pub const LOCK_FILE: &str = "record.lock";

/// 写入令牌的铸造权。
///
/// 元组字段私有，模块外无法构造；持有者才能铸造 [`Recorded`]。
///
/// [`Recorded`]: super::token::Recorded
pub struct Mint(SessionId);

impl Mint {
    pub(crate) fn new() -> Self {
        Self(SessionId::next())
    }

    /// 本次会话的标识；令牌携带它，写通道据此拒绝跨会话的令牌。
    pub(crate) const fn session(&self) -> SessionId {
        self.0
    }
}

/// 一次写目标字节的事务现场。
pub struct RecordSession<'a> {
    platform: &'a dyn Platform,
    directory: PathBuf,
    file: PathBuf,
    _lock: Box<dyn StateLock>,
    mint: Mint,
    /// 本次事务绑定的目标身份，同时用于记录载荷与目录定位。
    bound: Fingerprint,
    /// 已有的未终结记录所绑定的摘要。
    pending: Option<[u8; 32]>,
    /// 复用的记录已有这么多帧。
    sequence: u32,
    registered: bool,
}

impl<'a> RecordSession<'a> {
    /// 取目标记录目录的独占锁，并读出仍在进行中的记录（若有）。
    pub fn prepare(platform: &'a dyn Platform, identity: Fingerprint) -> Result<Self, ExecError> {
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
        let existing = match platform.read_record(&file).map_err(ExecError::from)? {
            RecordEntry::Contents(bytes) => Some(bytes),
            RecordEntry::Absent => None,
            RecordEntry::NotARecoveryFile => {
                return Err(ExecError::Record(format!(
                    "{} is not a recovery file",
                    file.display()
                )));
            }
        };
        if let Some(bytes) = existing
            && !bytes.is_empty()
        {
            let decoded =
                record::decode(&bytes).map_err(|error| ExecError::Record(error.to_string()))?;
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
            directory,
            file,
            _lock: lock,
            mint: Mint::new(),
            bound: identity,
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
            geometry_generation: 0,
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
            .write_state_file(&self.file, &encoded)
            .map_err(ExecError::from)?;
        self.platform
            .sync_directory(&self.directory)
            .map_err(ExecError::from)?;
        self.sequence = 2;
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
            geometry_generation: 0,
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
            .write_state_file(&self.file, &encoded)
            .map_err(ExecError::from)?;
        self.platform
            .sync_directory(&self.directory)
            .map_err(ExecError::from)?;
        self.sequence = 1;
        self.registered = true;
        Ok(())
    }

    /// 本步原像已落盘，可以写目标字节。
    pub fn token(&self) -> Recorded {
        Recorded::mint(&self.mint, u64::from(self.sequence))
    }

    /// 本会话的标识；写通道用它核对令牌来源。
    pub const fn session(&self) -> SessionId {
        self.mint.session()
    }

    /// 追加提交帧、落盘并删除记录。
    pub fn commit(&mut self) -> Result<(), ExecError> {
        if !self.registered {
            return Err(ExecError::Record("no preimage was registered".to_owned()));
        }
        let mut encoded = Vec::new();
        Frame {
            seq: self.sequence,
            kind: FrameKind::Committed,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: Vec::new(),
        }
        .encode_into(&mut encoded)
        .map_err(|error| ExecError::Record(error.to_string()))?;
        self.platform
            .append_state_file(&self.file, &encoded)
            .map_err(ExecError::from)?;
        self.platform
            .remove_state_file(&self.file)
            .map_err(ExecError::from)?;
        self.platform
            .sync_directory(&self.directory)
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
    use super::*;

    // covers: I4
    #[test]
    fn mint_grants_recorded_tokens() {
        let mint = Mint::new();
        let token = Recorded::mint(&mint, 7);
        assert_eq!(token.sequence(), 7);
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
