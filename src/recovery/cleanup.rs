//! 提交与清理：六条合取的落点。
//!
//! [`RecoveryEngine::commit`] 是唯一的签发点：它产出 [`CommittedRecord`]，并在留存、
//! 身份与表写位置都允许时附带 [`CleanupPermit`]。[`RecoveryEngine::cleanup`] 只做记录侧
//! 动作（删除 + 目录 fsync），准入判定不在这里重做。
//!
//! 六条在现有记录模型里的承载：
//!
//! | 条件 | 落点 |
//! | --- | --- |
//! | `Recognized` | [`Evidence::Recognized`]；否则 [`CommitRefusal::NotRecognized`] |
//! | `commit == Committed` | 产出值固定携带该状态；持久化由调用方在此之前完成 |
//! | 身份相符 | 删除前回读：从即将被删除的句柄读回字节，`Evidence::classify` 已识别且 `tx_id` 相等；否则 [`CleanupError::ReadbackMismatch`]，不删、保留记录 |
//! | `persist == Confirmed` | 记录没有独立字段；`Committed` 只在整份替换落盘后写入 |
//! | 后置回读通过 | 记录模型没有承载位，不判定 |
//! | `plan_index` 之后的表写步骤 | 调用方以 `table_change_pending` 报告 |
//!
//! 未确定：上表后两行的映射是从现有 `Record` 字段反推的——记录状态机写的是六项，
//! 而 `Record` 只留下 `commit` / `retention` / `stages` / `progress`，没有
//! `persist` 与 `plan_index` 的同名字段。

use crate::platform::{DirDurability, DirFd, EvidenceFile, EvidenceIo, EvidenceStore};

use super::evidence::Evidence;
use super::record::{CommitState, Record, Retention, TxId};

/// 已提交的记录。只由 [`RecoveryEngine::commit`] 产出，字段私有
#[derive(Debug)]
pub(crate) struct CommittedRecord {
    record: Record,
}

impl CommittedRecord {
    #[cfg(test)]
    pub(crate) fn record(&self) -> &Record {
        &self.record
    }
}

/// 清理许可。不可克隆、按值消费，只由 [`RecoveryEngine::commit`] 签发，
/// 且绑定签发时那份记录的事务标识
#[derive(Debug)]
pub(crate) struct CleanupPermit {
    tx_id: TxId,
}

/// 不给许可的原因，随 [`CommitOutcome`] 一起交付，供拒绝信息说明差在哪一条
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoPermitReason {
    /// 留存状态不允许清理（`Unverified` / `Quarantined`）
    Retention(Retention),
    /// `plan_index` 之后还有表写步骤没有落地
    PendingTableChange,
}

/// 提交被拒：证据本身不构成可提交的记录
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitRefusal {
    /// 证据不是一份完整可识别的记录
    NotRecognized,
}

/// 清理失败：I/O 故障，或删除前回读与待删记录不符
#[derive(Debug)]
pub(crate) enum CleanupError {
    Io(EvidenceIo),
    /// 回读字节不是这份记录：不删、保留。码值由调用方按本次是否已写目标字节分流
    /// （已写落 5：`commit_and_discard` 与回放写回后的 `settle_companion`；未写落 3）
    ReadbackMismatch(String),
}

impl From<EvidenceIo> for CleanupError {
    fn from(e: EvidenceIo) -> Self {
        Self::Io(e)
    }
}

/// 提交判定结果：记录已转为 `Committed`，许可要么给出，要么附带不给的理由
#[derive(Debug)]
pub(crate) struct CommitOutcome {
    committed: CommittedRecord,
    permit: Option<CleanupPermit>,
    withheld: Option<NoPermitReason>,
}

impl CommitOutcome {
    #[cfg(test)]
    pub(crate) fn committed(&self) -> &CommittedRecord {
        &self.committed
    }

    #[cfg(test)]
    pub(crate) fn permit(&self) -> Option<&CleanupPermit> {
        self.permit.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn withheld(&self) -> Option<NoPermitReason> {
        self.withheld
    }

    pub(crate) fn into_parts(
        self,
    ) -> (CommittedRecord, Option<CleanupPermit>, Option<NoPermitReason>) {
        (self.committed, self.permit, self.withheld)
    }
}

/// 提交与清理的判定入口
pub(crate) struct RecoveryEngine;

impl RecoveryEngine {
    /// 提交：把识别出的记录转为 `Committed`，并在留存状态允许、
    /// `plan_index` 之后没有表写步骤时签发清理许可。
    ///
    /// 对已经 `Committed` 的记录再次调用只重新签发许可，不改变状态
    pub(crate) fn commit(
        evidence: Evidence,
        table_change_pending: bool,
    ) -> Result<CommitOutcome, CommitRefusal> {
        let Evidence::Recognized(record) = evidence else {
            return Err(CommitRefusal::NotRecognized);
        };
        let mut record = record;
        record.commit = CommitState::Committed;
        let withheld = if !record.retention.allows_cleanup() {
            Some(NoPermitReason::Retention(record.retention))
        } else if table_change_pending {
            Some(NoPermitReason::PendingTableChange)
        } else {
            None
        };
        let permit = withheld.is_none().then_some(CleanupPermit { tx_id: record.tx_id });
        Ok(CommitOutcome {
            committed: CommittedRecord { record },
            permit,
            withheld,
        })
    }

    /// 清理：删除已提交的记录文件并让目录项持久化。删除前从句柄读回字节，
    /// 分类已识别且 `tx_id` 与待删记录相等才删；不符则不删、保留记录。
    /// 许可与记录不匹配是内部不变量破坏，同样拒绝。
    ///
    /// 未确定：把"许可与记录不匹配"记为 D7（内部不变量破坏），但端口
    /// [`EvidenceStore::remove`] 的返回类型只有 [`EvidenceIo`]，此处只能以
    /// `EvidenceIo::Unsupported` 拒绝，落不到 D7
    pub(crate) fn cleanup<S: EvidenceStore>(
        store: &S,
        anchor: &DirFd,
        file: EvidenceFile,
        committed: &CommittedRecord,
        permit: CleanupPermit,
    ) -> Result<DirDurability, CleanupError> {
        if permit.tx_id != committed.record.tx_id {
            return Err(EvidenceIo::Unsupported(
                "the cleanup permit was issued for another record; the deletion is refused"
                    .to_string(),
            )
            .into());
        }
        let bytes = store.read(&file)?;
        match Evidence::classify(&bytes, &committed.record.target) {
            Evidence::Recognized(read_back) if read_back.tx_id == committed.record.tx_id => {}
            Evidence::Recognized(_) => {
                return Err(CleanupError::ReadbackMismatch(
                    "the record read back before deletion carries a different transaction id; \
                     the record is kept"
                        .to_string(),
                ));
            }
            _ => {
                return Err(CleanupError::ReadbackMismatch(
                    "the bytes read back before deletion are no longer a recognized record; \
                     the record is kept"
                        .to_string(),
                ));
            }
        }
        store.remove(anchor, file, committed, &permit).map_err(CleanupError::Io)
    }
}
