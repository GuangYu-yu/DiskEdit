//! 计划步骤：步骤种类、访问级别与涉及的字节区间。
//!
//! 访问级别由步骤种类唯一决定，不单独传入，避免声明与实际作用不符。

use std::path::PathBuf;

use diskedit_core::fs::{ContentKind, FsKind, MkfsKind};
use diskedit_core::region::Region;
use diskedit_core::table::Guid;

use crate::app::observe::{ContentProperty, RecoverMode, TableFormat};

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

/// 内容属性要写的值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyValue {
    /// 明确的文本：卷标，或调用方给定的 UUID。
    Text(String),
    /// 让目标生成新的 UUID（ntfs 的 serial 只有这一种可写形式）。
    Generate,
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
    /// 修改分区内部的文件系统属性（卷标或 UUID）。
    ///
    /// 属性值只有两种落地形态：明确的文本，或让目标自己生成新的 UUID。后者是
    /// 目标自带的生成能力，计划里保留这个意图而不是先造一个随机值，重跑才稳定。
    SetProperty {
        partition: u32,
        property: ContentProperty,
        value: PropertyValue,
        /// 内容属性作用的内容类型。
        content: ContentKind,
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

/// 一次搬移的带符号位移（扇区）。
///
/// 只能由新旧起点导出：表示不了（超出 `i64`）或没有位移都返回 `None`，调用方
/// 拒绝，不把差值截断成边界值。目标块由源块加该位移得到。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Displacement {
    shift: i64,
}

impl Displacement {
    /// 新旧起点之差。
    pub fn between(old_start: u64, new_start: u64) -> Option<Self> {
        let shift = i64::try_from(i128::from(new_start) - i128::from(old_start)).ok()?;
        if shift == 0 {
            return None;
        }
        Some(Self { shift })
    }

    /// 带符号的扇区数。
    pub const fn sectors(self) -> i64 {
        self.shift
    }

    /// 目标区间是否与源区间相交：位移量小于长度即自重叠。
    pub fn overlaps(self, sectors: u64) -> bool {
        self.shift.unsigned_abs() < sectors
    }
}

/// 字节换算成扇区，向下取整；扇区大小为零即 `None`。
pub fn sectors_floor(bytes: u64, sector_size: u64) -> Option<u64> {
    if sector_size == 0 {
        return None;
    }
    Some(bytes / sector_size)
}

/// 字节换算成扇区，向上取整：不足一个扇区也算一整个。
pub fn sectors_ceil(bytes: u64, sector_size: u64) -> Option<u64> {
    if sector_size == 0 {
        return None;
    }
    Some(bytes.div_ceil(sector_size))
}

/// 扇区数换算成字节数；溢出即 `None`。
pub fn sectors_bytes(sectors: u64, sector_size: u64) -> Option<u64> {
    if sector_size == 0 {
        return None;
    }
    sectors.checked_mul(sector_size)
}

/// 起点 LBA 换算成字节偏移；溢出即 `None`。
pub fn lba_bytes(lba: u64, sector_size: u64) -> Option<u64> {
    sectors_bytes(lba, sector_size)
}

/// 含两端的 LBA 区间有多少个扇区；区间倒置即 `None`。
pub fn span_sectors(start_lba: u64, end_lba: u64) -> Option<u64> {
    end_lba.checked_sub(start_lba)?.checked_add(1)
}

/// 含两端的 LBA 区间有多少字节；区间倒置或溢出即 `None`。
pub fn span_bytes(start_lba: u64, end_lba: u64, sector_size: u64) -> Option<u64> {
    sectors_bytes(span_sectors(start_lba, end_lba)?, sector_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_and_sector_conversions_are_one_explicit_point() {
        // 非整倍：向下取整与向上取整都必须说得清，不能靠调用点各自截断。
        assert_eq!(sectors_floor(1000, 512), Some(1));
        assert_eq!(sectors_ceil(1000, 512), Some(2));
        assert_eq!(sectors_floor(1024, 512), Some(2));
        assert_eq!(sectors_bytes(2, 512), Some(1024));
        assert_eq!(span_sectors(10, 19), Some(10));
        assert_eq!(span_bytes(10, 19, 512), Some(5120));
        // 零扇区无法换算：调用方必须拒绝，不能拿零当除数。
        assert_eq!(sectors_floor(1000, 0), None);
        assert_eq!(sectors_ceil(1000, 0), None);
        assert_eq!(sectors_bytes(1, 0), None);
        // 极值溢出即 `None`，不回绕。
        assert_eq!(sectors_bytes(u64::MAX, 2), None);
        assert_eq!(span_bytes(0, u64::MAX, 2), None);
        // 区间倒置不是"零长度"。
        assert_eq!(span_sectors(10, 9), None);
    }

    #[test]
    fn a_forward_displacement_is_positive_and_overlapping() {
        let displacement = Displacement::between(10, 25).expect("正向位移可表示");
        assert_eq!(displacement.sectors(), 15);
        // 位移 15 小于长度 100：源与目标重叠。
        assert!(displacement.overlaps(100));
        assert!(!displacement.overlaps(15));
    }

    #[test]
    fn a_backward_displacement_is_negative_and_overlapping() {
        let displacement = Displacement::between(25, 10).expect("负向位移可表示");
        assert_eq!(displacement.sectors(), -15);
        assert!(displacement.overlaps(100));
        assert!(!displacement.overlaps(15));
    }

    #[test]
    fn an_unrepresentable_or_zero_displacement_is_refused() {
        // 零位移不是搬移：调用方应给出"无需移动"而不是造一个空步骤。
        assert!(Displacement::between(7, 7).is_none());
        // 差值超出 `i64`：拒绝，不截断成 `i64::MAX`。
        assert!(Displacement::between(0, u64::MAX).is_none());
        assert!(Displacement::between(u64::MAX, 0).is_none());
    }
}
