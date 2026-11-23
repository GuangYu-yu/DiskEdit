//! 恢复引擎：读记录、按纯恢复器的结论选择动作。
//!
//! 纯恢复器只由记录字节与当前身份给出结论（`Absent` / `Committed` /
//! `RollBack` / `Replay` / `Foreign` / `NeedsManual`）。本模块把结论落到动作：
//! 已提交或已放弃的清理记录；完整前缀回滚表区原像后清理；外来格式与证据冲突
//! 一律拒绝且不改目标字节。`Replay` 表示最后一步已完整落盘但事务未收尾，在
//! 无法重算剩余步骤时同样按回滚处理，目标回到执行前的表区状态。

use std::path::Path;

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{RecoveryPlan, recover};

use crate::app::execute::recover::{abandon, cleanup, list, rollback};
use crate::platform::{Platform, RecordStore, TargetHandle};

/// 恢复动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 回滚表区原像并清理记录。
    RollBack,
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
    /// 回滚了给定数量的表区原像并清理记录。
    RolledBack { regions: usize },
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

    fn rolled_back(regions: usize) -> Self {
        Self {
            action: Action::RolledBack { regions },
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
        Mode::RollBack => rollback_record(&mut *store, handle, identity),
    }
}

fn rollback_record(
    store: &mut dyn RecordStore,
    target: &mut dyn TargetHandle,
    identity: Fingerprint,
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
            "记录不是本实现写下的：{}",
            String::from_utf8_lossy(&magic)
        ))),
        RecoveryPlan::NeedsManual { why } => Err(RecoveryFailure::Refused(why)),
        RecoveryPlan::RollBack { frames } | RecoveryPlan::Replay { frames } => {
            let rolled = rollback::apply(target, &frames).map_err(|failure| {
                let why = failure.error.to_string();
                if failure.written == 0 {
                    RecoveryFailure::Refused(why)
                } else {
                    RecoveryFailure::Interrupted(why)
                }
            })?;
            cleanup::clear(store).map_err(|error| {
                RecoveryFailure::Refused(format!("回滚已完成但清理记录失败：{error}"))
            })?;
            Ok(Summary::rolled_back(rolled.regions))
        }
    }
}
