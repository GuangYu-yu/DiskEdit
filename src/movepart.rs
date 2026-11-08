//! 分区搬移与中间分区扩容闭环。
//!
//! 前向恢复、无回滚；单分区 = 单事务；进度由恢复记录承载（一次 payload 写入即一个
//! durable 边界）。
//!
//! 本模块只留判定：条目怎么排（[`plan`]）、扩容收尾的区间与分类（[`post`]）、
//! 与搬移共用的几何判定（[`resize`]）。数据搬移、表提交与换区重建都在
//! [`crate::execution`]：装配层把这些判定写成步骤序列，执行器按同一份步骤写盘。
//! 两条命令（`move` 与 `resize --allow-move`）因此只有一条写盘路径。

mod faults;
mod post;
mod plan;
mod shift;
mod resize;
mod copy;

#[cfg(test)]
mod tests;

pub(crate) use faults::*;
pub(crate) use post::*;
pub(crate) use plan::*;
pub(crate) use shift::*;
pub(crate) use resize::*;
pub(crate) use copy::*;

use crate::dev::{FileSource, WriteSource};
use crate::fsid::ContentKind;
use crate::fsops::Growable;
use crate::geometry::ValidatedGeometry;
use crate::gpt_policy::{self, RepairAction};
use crate::outcome::{RunOutcome, Pending, PendingKind};
use crate::table;
use std::io;