//! 判定层：把请求与事实快照变成只读报告、可执行计划或一条拒绝。
//!
//! 判定只做纯计算，不读盘、不运行外部程序。计划在这里成形并被摘要绑定，
//! 执行层只接受已确认的计划值。

pub mod check;
pub mod confirmed;
pub mod digest;
pub mod edit;
pub mod filesystem;
pub mod layout;
pub mod mkfs;
pub mod new;
pub mod plan;
pub mod records;
pub mod refusal;
pub mod release;
pub mod repair;
pub mod replay;
pub mod report;
pub mod resize;
pub mod step;
pub mod undo;

pub use confirmed::{ConfirmedPlan, confirm};
pub use plan::Plan;
pub use refusal::Refusal;
pub use report::Report;
pub use step::{Access, PropertyValue, Step, StepKind};

use crate::platform::{Occupancy, PartitionMount};

/// 1 MiB 的字节数。
pub(crate) const MIB: u64 = 1024 * 1024;

/// 未给出 `--chunk` 时搬移/复制内容的块大小。
pub(crate) const DEFAULT_CHUNK: u64 = 4 << 20;

/// 计划期读到的一次占用现场。
///
/// `scope` 说明占用结论是按哪个对象探测的；`occupancy` 是当时的结论，其中
/// [`Occupancy::Unknown`] 表示读不出，不是空闲。设备形态下另记所选分区的挂载与
/// swap 现场，未涉及单个分区时为 `None`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OccupancySnapshot {
    pub scope: OccupancyScope,
    pub occupancy: Occupancy,
    pub partition: Option<PartitionUsage>,
}

/// 计划期占用结论的探测对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OccupancyScope {
    /// 目标本身（整盘或镜像）。
    Target,
    /// 目标本身，按 LVM 收尾口径：DM 栈持有的 loop 不算占用。
    TargetLvm,
    /// 目标本身优先；其结论为空闲时取该分区的设备节点（PV 的分区级持有者）。
    TargetThenPartition { number: u32 },
}

/// 计划期所选分区的挂载与 swap 现场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionUsage {
    pub number: u32,
    pub mount: PartitionMount,
    pub swap_active: bool,
}

/// 一次判定的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// 只读命令的报告与审计计划。
    Report { plan: Plan, report: Report },
    /// 会改变目标或外部状态的计划，执行前需确认。
    Execute {
        plan: Plan,
        /// 成功后的唯一一句人读摘要。
        note: String,
    },
    /// 请求已经是目标状态，无需写入。
    Unchanged { note: String },
    /// 拒绝，并带上固定成因。
    Refuse(Refusal),
}
