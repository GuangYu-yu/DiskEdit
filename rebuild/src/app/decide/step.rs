//! 计划步骤：步骤种类、访问级别与涉及的字节区间。
//!
//! 访问级别由步骤种类唯一决定，不单独传入，避免声明与实际作用不符。

use std::path::PathBuf;

use diskedit_core::fs::{ContentKind, FsKind, MkfsKind};
use diskedit_core::region::Region;
use diskedit_core::table::Guid;

use crate::app::observe::{RecoverMode, SetProperty, TableFormat};

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

/// 一段完整的表写入：区间与要落盘的字节。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSegment {
    pub region: Region,
    pub bytes: Vec<u8>,
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
    WriteTable {
        format: TableFormat,
        segment_size: u64,
        /// 写前表区摘要：执行前重读首/中/末三扇区必须得到同一个值。
        stamp: [u8; 32],
        /// 按落盘顺序排列的写段；`regions` 是本步声明的全部区间。
        segments: Vec<TableSegment>,
    },
    /// 新增一个分区。
    AddPartition {
        format: TableFormat,
        start_lba: u64,
        sectors: u64,
        type_guid: Guid,
        name: String,
        /// 指定的条目槽位；`None` 表示第一个空槽。
        part_number: Option<u32>,
        /// 指定的分区唯一 GUID；`None` 由计划摘要派生。
        unique: Option<Guid>,
    },
    /// 删除一个分区。
    DeletePartition { format: TableFormat, partition: u32 },
    /// 改变一个分区的起止。
    ResizePartition {
        format: TableFormat,
        partition: u32,
        start_lba: u64,
        sectors: u64,
    },
    /// 修改分区或文件系统属性。
    SetProperty {
        partition: u32,
        property: SetProperty,
        value: String,
        /// 内容属性的内容类型；表属性为 `None`。
        content: Option<ContentKind>,
    },
    /// 在分区上建卷。
    MakeFilesystem {
        partition: u32,
        filesystem: MkfsKind,
        label: Option<String>,
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
    /// 按新尺寸重建 swap 区域，保留旧首部的 UUID 与卷标。
    ///
    /// swap 没有原地 resize：分区表落位之后，新尺寸只有重建才生效。
    RebuildSwap {
        partition: u32,
        /// 分区起点的字节偏移，用于读取旧首部与镜像经 loop 绑定的偏移。
        start_bytes: u64,
        /// 重建后的分区字节数。
        bytes: u64,
    },
    /// 在已挂载的文件系统上扩容到所在分区的容量。
    ///
    /// 不写分区表、不登记原像、不卸载用户挂载；由在线执行器运行该族的在线工具。
    GrowFilesystemOnline {
        mount_point: PathBuf,
        device: PathBuf,
        disk: PathBuf,
        partition: u32,
        filesystem: FsKind,
        bytes: u64,
    },
    /// 在已挂载的文件系统上缩容到指定字节数。
    ///
    /// 只用于支持在线收缩的族（btrfs）；必须在写表与调整内核视图之前执行，
    /// 否则分区先缩、文件系统后缩会让内核看到越界。
    ShrinkFilesystemOnline {
        mount_point: PathBuf,
        device: PathBuf,
        disk: PathBuf,
        partition: u32,
        filesystem: FsKind,
        bytes: u64,
    },
    /// 缩容前把 LVM2 PV 的元数据缩到指定字节数。
    ///
    /// 必须在写表之前执行：PV 元数据先让出尾部，表再缩到同一处。
    LvmPvShrink {
        partition: u32,
        /// 分区起点的字节偏移，用于镜像经 loop 绑定的偏移。
        start_bytes: u64,
        /// 缩后的分区字节数。
        bytes: u64,
    },
    /// 扩容后让 PV 吸收新增空间，并按需扩展目标 LV 与其文件系统。
    LvmGrowChain {
        partition: u32,
        start_bytes: u64,
        old_bytes: u64,
        new_bytes: u64,
        grow_lv: bool,
        lv: Option<String>,
    },
    /// 把源分区内容复制到给定的目标区间。
    CopyPartition {
        source: u32,
        source_lba: u64,
        start_lba: u64,
        sectors: u64,
        chunk: u64,
    },
    /// 平移一个分区的内容。
    MovePartition {
        partition: u32,
        source_lba: u64,
        offset_sectors: i64,
        chunk: u64,
        /// 目标内容已识别为 NTFS：平移后修正主备引导扇区里的 `HiddenSectors`。
        ntfs_hidden: bool,
    },
    /// 让内核按新的起点与长度调整一个已存在分区（`BLKPG_RESIZE_PARTITION`）。
    ///
    /// 用于已挂载分区：整盘重读在分区被占用时返回 `EBUSY`，只能按分区增量调整。
    RefreshKernelView {
        disk: PathBuf,
        partition: u32,
        start_bytes: u64,
        bytes: u64,
    },
    /// 处理记录：回滚、重放或归档。
    RecoverRecords { mode: RecoverMode },
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
            Self::RefreshKernelView { .. }
            | Self::GrowFilesystemOnline { .. }
            | Self::ShrinkFilesystemOnline { .. } => Access::WritesExternalState,
            Self::WriteTable { .. }
            | Self::AddPartition { .. }
            | Self::DeletePartition { .. }
            | Self::ResizePartition { .. }
            | Self::SetProperty { .. }
            | Self::MakeFilesystem { .. }
            | Self::GrowFilesystem { .. }
            | Self::ShrinkFilesystem { .. }
            | Self::RebuildSwap { .. }
            | Self::CopyPartition { .. }
            | Self::RecoverRecords { .. }
            | Self::LvmPvShrink { .. }
            | Self::LvmGrowChain { .. }
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
