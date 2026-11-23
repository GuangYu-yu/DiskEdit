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
    /// 所需外部程序不可用。
    ToolMissing { program: Program },
    /// 当前平台不提供外部程序。
    PlatformCannotRunTools { why: String },
    /// 参数取值超出允许范围。
    ArgumentOutOfRange { what: String },
}

impl Refusal {
    /// 固定的下一步文案。
    pub fn next_step(&self) -> &'static str {
        match self {
            Self::ConfirmationRequired => "确认后以 --yes 重新执行",
            Self::OverwriteNotConfirmed => "目标不是空盘；确认覆盖后加覆盖旗标重新执行",
            Self::IdentityUnprovable { .. } => "确认目标是块设备整盘、分区或镜像文件后重试",
            Self::IdentityChanged => "目标在计划之后发生变化，重新运行以重新观察",
            Self::PlanDigestMismatch => "计划不一致，放弃本次执行并重新生成计划",
            Self::OccupancyUnknown { .. } => "先解除挂载或释放占用，再查看占用来源后重试",
            Self::Occupied { .. } => "先卸载或释放占用，再重试；只读命令可直接使用",
            Self::FactUnknown { .. } => "补充探测条件后重新运行；未知事实不会被当作默认值",
            Self::GeometryUnknown { .. } => "确认目标逻辑块大小与容量后重试",
            Self::TableMissing => "先创建分区表，或将命令指向已有分区表的目标",
            Self::TableDamaged { .. } => "先用只读命令检查证据，必要时从备份副本恢复",
            Self::PartitionMissing { .. } => "先用列举命令确认分区号",
            Self::PartitionOverlap { .. } => "调整起止位置，避免与既有分区重叠",
            Self::CapacityInsufficient { .. } => "缩小请求尺寸或换用更大的目标",
            Self::FilesystemUnknown { .. } => "先识别分区内容类别，再选择对应操作",
            Self::ContentUnsupported { .. } => "改用该内容类别支持的操作或外部工具",
            Self::MinimumSizeUnknown { .. } => "先在不缩小的前提下确认文件系统最小尺寸",
            Self::ToolMissing { .. } => "安装所需外部程序后重试",
            Self::PlatformCannotRunTools { .. } => "在提供所需外部程序的平台上重试",
            Self::ArgumentOutOfRange { .. } => "按帮助中的取值范围修正参数",
        }
    }
}
