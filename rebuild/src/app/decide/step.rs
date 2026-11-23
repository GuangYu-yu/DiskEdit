//! 计划步骤：步骤种类、访问级别与涉及的字节区间。
//!
//! 访问级别由步骤种类唯一决定，不单独传入，避免声明与实际作用不符。

use diskedit_core::fs::FsKind;
use diskedit_core::region::Region;

use super::TableFormat;

/// 步骤对目标的影响方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// 只读，不改变目标与外部状态。
    ReadOnly,
    /// 写目标字节。
    WritesTarget,
    /// 改变目标之外的状态，例如内核分区视图或记录目录。
    WritesExternalState,
}

/// 一步做什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// 渲染只读信息。
    Inspect,
    /// 检查一个分区。
    Check { partition: u32 },
    /// 列出活跃与隔离的记录。
    ListRecords,
    /// 校验一条记录。
    VerifyRecord { token: String },
    /// 扫描记录目录。
    ScanRecords,
    /// 写出完整分区表。
    WriteTable { format: TableFormat },
    /// 更新条目数组与头部校验。
    WriteEntries { format: TableFormat },
    /// 修改文件系统属性。
    SetAttributes { partition: u32 },
    /// 在分区上建卷。
    MakeFilesystem {
        partition: u32,
        filesystem: FsKind,
    },
    /// 扩展文件系统到指定字节数。
    GrowFilesystem {
        partition: u32,
        filesystem: FsKind,
        bytes: u64,
    },
    /// 收缩文件系统到指定字节数。
    ShrinkFilesystem {
        partition: u32,
        filesystem: FsKind,
        bytes: u64,
    },
    /// 复制一个分区的内容到另一个区间。
    CopyPartition { source: u32, target: u32 },
    /// 平移一个分区的内容。
    MovePartition { partition: u32, offset_sectors: i64 },
    /// 让内核重读分区表与块设备尺寸。
    RefreshKernelView,
    /// 处理记录：回滚、重放或归档。
    RecoverRecords,
}

impl StepKind {
    /// 该步骤的影响方式。
    pub const fn access(&self) -> Access {
        match self {
            Self::Inspect
            | Self::Check { .. }
            | Self::ListRecords
            | Self::VerifyRecord { .. }
            | Self::ScanRecords => Access::ReadOnly,
            Self::RefreshKernelView | Self::RecoverRecords => Access::WritesExternalState,
            Self::WriteTable { .. }
            | Self::WriteEntries { .. }
            | Self::SetAttributes { .. }
            | Self::MakeFilesystem { .. }
            | Self::GrowFilesystem { .. }
            | Self::ShrinkFilesystem { .. }
            | Self::CopyPartition { .. }
            | Self::MovePartition { .. } => Access::WritesTarget,
        }
    }
}

/// 一个计划步骤。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    pub regions: Vec<Region>,
}

impl Step {
    pub fn new(kind: StepKind, regions: Vec<Region>) -> Self {
        Self { kind, regions }
    }

    pub fn access(&self) -> Access {
        self.kind.access()
    }
}
