//! 恢复引擎：读记录、按纯恢复器的结论选择动作。
//!
//! 纯恢复器只由记录字节与当前身份给出结论（`Absent` / `Committed` /
//! `RollBack` / `Replay` / `Foreign` / `NeedsManual`）。本模块把结论落到动作：
//! 已提交或已放弃的清理记录；完整前缀回滚表区原像后清理；外来格式与证据冲突
//! 一律拒绝且不改目标字节。
//!
//! 两种回滚形态由此分开：
//!
//! - 默认 `recover` 只处理能证明数据区未动的记录。`RollBack`（最后一步未完整
//!   落盘）可以整段回滚；`Replay`（最后一步已完成）里若有已落盘的数据区步骤，
//!   数据可能已经移动，默认拒绝并指路 `--replay-table-only`。
//! - `--replay-table-only` 明确只回放分区表区原像，数据区保持现状。
//!
//! 两边都只写记录里携带原像的表区字节，写完逐条读回比对。

use std::path::Path;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{Frame, RecoveryPlan, recover};

use crate::app::execute::recover::rollback::{self, Scope};
use crate::app::execute::recover::{abandon, cleanup, list};
use crate::platform::{Platform, RecordStore, TargetHandle};

/// 恢复动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 回滚表区原像并清理记录；数据区步骤已完成时拒绝。
    RollBack,
    /// 只回放分区表区原像并清理记录，数据区保持现状。
    ReplayTableOnly,
    /// 放弃回滚：把记录移入隔离区，目标字节不变。
    Release,
    /// 只列出记录目录条目。
    List,
}

/// 实际执行的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// 没有记录，未做任何改动。
    Nothing,
    /// 清理了已提交或已放弃的记录。
    Cleaned,
    /// 回滚了给定数量的表区原像并清理记录；`irreversible` 为不回滚的数据区步骤数。
    RolledBack { regions: usize, irreversible: usize },
    /// 把记录移入隔离区。
    Released { token: String },
    /// 列出了给定数量的条目。
    Listed { entries: usize },
}

/// 一次恢复的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub action: Action,
}

impl Summary {
    fn nothing() -> Self {
        Self {
            action: Action::Nothing,
        }
    }

    fn cleaned() -> Self {
        Self {
            action: Action::Cleaned,
        }
    }

    fn rolled_back(regions: usize, irreversible: usize) -> Self {
        Self {
            action: Action::RolledBack {
                regions,
                irreversible,
            },
        }
    }
}

/// 恢复失败。
#[derive(Debug)]
pub enum RecoveryFailure {
    /// 未写目标字节的拒绝：记录缺失、外来格式、证据冲突或清理失败。
    Refused(String),
    /// 已写目标字节但未完成回滚。
    Interrupted(String),
    /// 本机不支持所需能力。
    Unsupported(String),
}

/// 执行一次恢复。
pub fn run(
    platform: &dyn Platform,
    target: &Path,
    identity: Fingerprint,
    handle: &mut dyn TargetHandle,
    mode: Mode,
) -> Result<Summary, RecoveryFailure> {
    let mut store = platform
        .records(target, identity)
        .map_err(|error| RecoveryFailure::Refused(error.to_string()))?;
    match mode {
        Mode::List => {
            let entries = list::entries(&*store)
                .map_err(|error| RecoveryFailure::Refused(error.to_string()))?;
            Ok(Summary {
                action: Action::Listed {
                    entries: entries.len(),
                },
            })
        }
        Mode::Release => {
            let token = abandon::apply(&mut *store)
                .map_err(|error| RecoveryFailure::Refused(error.to_string()))?;
            Ok(Summary {
                action: Action::Released { token },
            })
        }
        Mode::RollBack => rollback_record(&mut *store, handle, identity, Scope::All),
        Mode::ReplayTableOnly => rollback_record(&mut *store, handle, identity, Scope::TableOnly),
    }
}

fn rollback_record(
    store: &mut dyn RecordStore,
    target: &mut dyn TargetHandle,
    identity: Fingerprint,
    scope: Scope,
) -> Result<Summary, RecoveryFailure> {
    let bytes = store
        .load()
        .map_err(|error| RecoveryFailure::Refused(error.to_string()))?;
    let Some(bytes) = bytes else {
        return Ok(Summary::nothing());
    };
    match recover(&bytes, identity) {
        RecoveryPlan::Absent => Ok(Summary::nothing()),
        RecoveryPlan::Committed => {
            cleanup::clear(store).map_err(|error| RecoveryFailure::Refused(error.to_string()))?;
            Ok(Summary::cleaned())
        }
        RecoveryPlan::Foreign { magic } => Err(RecoveryFailure::Refused(format!(
            "record was not written by this implementation: {}",
            String::from_utf8_lossy(&magic)
        ))),
        RecoveryPlan::NeedsManual { why } => Err(RecoveryFailure::Refused(why)),
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            // 默认回滚不能越过已落盘的数据区步骤：数据可能已经移动，声称回滚
            // 会把"只恢复了表区"说成"全部复原"。
            if scope == Scope::All && data_may_have_moved(&frames) {
                return Err(RecoveryFailure::Refused(
                    "record has completed data-region steps; data may have moved and the default rollback cannot restore it; \
                     use recover --replay-table-only to restore only the partition table"
                        .to_owned(),
                ));
            }
            let rolled = rollback::apply(target, &frames, scope).map_err(|failure| {
                let why = failure.error.to_string();
                if failure.written == 0 {
                    RecoveryFailure::Refused(why)
                } else {
                    RecoveryFailure::Interrupted(why)
                }
            })?;
            cleanup::clear(store).map_err(|error| {
                RecoveryFailure::Refused(format!(
                    "rolled back but failed to clear the record: {error}"
                ))
            })?;
            Ok(Summary::rolled_back(rolled.regions, rolled.irreversible))
        }
    }
}

/// 记录里是否已有完成落盘的数据区步骤。
fn data_may_have_moved(frames: &[Frame]) -> bool {
    frames
        .iter()
        .any(|frame| frame.completed && rollback::intent_is(&frame.intent, rollback::DATA_INTENTS))
}

#[cfg(test)]
mod tests {
    use diskedit_core::record::FrameKind;

    use super::*;

    fn frame(intent: &str, completed: bool) -> Frame {
        Frame {
            seq: 0,
            kind: FrameKind::Progress,
            completed,
            irreversible: false,
            intent: intent.as_bytes().to_vec(),
            regions: Vec::new(),
        }
    }

    #[test]
    fn completed_data_step_blocks_default_rollback() {
        assert!(data_may_have_moved(&[frame("grow-filesystem", true)]));
        assert!(data_may_have_moved(&[
            frame("write-table", true),
            frame("move-partition", true),
        ]));
    }

    #[test]
    fn table_only_or_incomplete_steps_do_not_block() {
        assert!(!data_may_have_moved(&[frame("write-table", true)]));
        assert!(!data_may_have_moved(&[frame("grow-filesystem", false)]));
        assert!(!data_may_have_moved(&[]));
    }
}
