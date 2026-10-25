//! 分区搬移与中间分区扩容闭环。
//!
//! 前向恢复、无回滚；单分区 = 单事务；checkpoint 原子写
//! （tmp → sync → rename → fsync 父目录，其中父目录 fsync 仅 unix 实现——
//! Windows 上承诺降级为 rename 语义，不经此路径写盘所以不影响恢复协议）；
//! 复制方向：delta ≥ 0 且尾→头推进，
//! 写点恒在未读源之上（右移时目的地址总是大于已读位置），顺序固化不提供方向参数。

mod faults;
mod plan;
mod checkpoint;
mod shift;
mod resize;
mod copy;

#[cfg(test)]
mod tests;

pub(crate) use faults::*;
pub(crate) use plan::*;
pub(crate) use checkpoint::*;
pub(crate) use shift::*;
pub(crate) use resize::*;
pub(crate) use copy::*;

use crate::dev::{FileSource, WriteSource};
use crate::fsid::FsKind;
use crate::fsops::Growable;
use crate::geometry::{GeometryLimits, ValidatedGeometry};
use crate::gpt_policy::{self, RepairAction};
use crate::outcome::{Fail, Outcome, Pending, PendingKind, PostWrite};
use crate::table;
use std::io;
use std::path::PathBuf;