//! 拒绝成因的闭集与各自固定的下一步文案。
//!
//! 每个成因只有一条文案，新增变体不写文案就编译失败。退出码不在这里产生，
//! 由报告层按成因分类统一映射。

use diskedit_core::fs::Program;

/// 一次判定为什么拒绝。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 写命令缺少确认旗标。
    ConfirmationRequired,
    /// 目标已有可识别的分区表或内容，覆盖需要显式旗标。
    OverwriteNotConfirmed,
    /// 目标身份读不到或等级不足。
    IdentityUnprovable { why: String },
    /// 目标与计划所指不是同一个对象。
    IdentityChanged,
    /// 计划摘要与步骤不符，内部不一致。
    PlanDigestMismatch,
    /// 占用状态未知，不能证明目标空闲。
    OccupancyUnknown { why: String },
    /// 目标被占用。
    Occupied { evidence: String },
    /// 判定所需的事实未知。
    FactUnknown { what: String },
    /// 目标几何未知或超出可构造范围。
    GeometryUnknown { why: String },
    /// 目标上没有可用分区表。
    TableMissing,
    /// 分区表损坏。
    TableDamaged { why: String },
    /// 指定的分区不存在。
    PartitionMissing { number: u32 },
    /// 新分区与既有分区重叠。
    PartitionOverlap { number: u32 },
    /// 目标区间放不下请求的大小。
    CapacityInsufficient { needed: u64, available: u64 },
    /// 分区内容类别未知。
    FilesystemUnknown { number: u32 },
    /// 该内容类别不支持该操作。
    ContentUnsupported { what: String, why: String },
    /// 所需的文件系统最小尺寸无法证明。
    MinimumSizeUnknown { number: u32, why: String },
    /// 请求的大小小于文件系统可收缩到的最小尺寸。
    BelowMinimumSize {
        number: u32,
        minimum: u64,
        requested: u64,
    },
    /// 所需外部程序不可用。
    ToolMissing { program: Program },
    /// 当前平台不提供外部程序。
    PlatformCannotRunTools { why: String },
    /// 参数取值超出允许范围。
    ArgumentOutOfRange { what: String },
    /// 旗标名或旗标适用形态不在支持集合内。
    FlagUnknown { why: String },
}

impl Refusal {
    /// 固定的下一步文案。
    pub fn next_step(&self) -> &'static str {
        match self {
            Self::ConfirmationRequired => "rerun with --yes after confirmation",
            Self::OverwriteNotConfirmed => {
                "target is not blank; rerun with the overwrite flag after confirming overwrite"
            }
            Self::IdentityUnprovable { .. } => {
                "confirm the target is a whole block device, a partition, or an image file, then retry"
            }
            Self::IdentityChanged => "target changed after planning; rerun to observe it again",
            Self::PlanDigestMismatch => "plan mismatch; abandon this run and regenerate the plan",
            Self::OccupancyUnknown { .. } => {
                "unmount or release the usage first, then inspect the usage source and retry"
            }
            Self::Occupied { .. } => {
                "unmount or release the usage first, then retry; read-only commands can be used directly"
            }
            Self::FactUnknown { .. } => {
                "provide the missing probe conditions and rerun; unknown facts are never treated as defaults"
            }
            Self::GeometryUnknown { .. } => {
                "confirm the target logical block size and capacity, then retry"
            }
            Self::TableMissing => {
                "create a partition table first, or point the command at a target that already has one"
            }
            Self::TableDamaged { .. } => {
                "inspect the evidence with read-only commands first, and recover from a backup copy if necessary"
            }
            Self::PartitionMissing { .. } => {
                "confirm the partition number with a listing command first"
            }
            Self::PartitionOverlap { .. } => {
                "adjust the start and end so they do not overlap existing partitions"
            }
            Self::CapacityInsufficient { .. } => "shrink the requested size or use a larger target",
            Self::FilesystemUnknown { .. } => {
                "identify the partition content kind first, then choose the matching operation"
            }
            Self::ContentUnsupported { .. } => {
                "use an operation or external tool supported by this content kind"
            }
            Self::MinimumSizeUnknown { .. } => {
                "confirm the filesystem minimum size before shrinking"
            }
            Self::BelowMinimumSize { .. } => {
                "raise the target size above the filesystem minimum size"
            }
            Self::ToolMissing { .. } => "install the required external program and retry",
            Self::PlatformCannotRunTools { .. } => {
                "retry on a platform that provides the required external programs"
            }
            Self::ArgumentOutOfRange { .. } => {
                "fix the argument according to the valid range in the help"
            }
            Self::FlagUnknown { .. } => "use one of the accepted flag names for this table",
        }
    }
}
