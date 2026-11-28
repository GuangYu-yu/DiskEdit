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
    /// 挂载点形态的目录不是挂载点。
    MountNotMounted { path: String },
    /// 同一路径有多条挂载记录，无法确定生效层。
    MountAmbiguous { path: String },
    /// 挂载设备存在，但不是分区。
    MountNotPartition { device: String },
    /// 挂载源不是块设备（多设备或 bind 挂载）。
    MountNotBlockDevice { source: String },
    /// 无法把挂载映射到单一块设备。
    MountUnmapped { reason: String },
    /// 在线形态只扩容，不收窄文件系统。
    OnlineShrink { why: String },
    /// 分区表损坏。
    TableDamaged { why: String },
    /// 该形态需要 GPT 分区表。
    GptRequired { what: String },
    /// 指定的记录不存在。
    RecordNotFound { token: String },
    /// 记录的锚定目录无法打开。
    RecordStoreMissing { path: String },
    /// 目标自身给不出记录目录的键。
    RecordDirectoryUnresolved { target: String },
    /// 指定的分区不存在。
    PartitionMissing { number: u32 },
    /// 新分区与既有分区重叠。
    PartitionOverlap { number: u32 },
    /// 目标区间放不下请求的大小。
    CapacityInsufficient { needed: u64, available: u64 },
    /// 分区内容类别未知。
    FilesystemUnknown { number: u32 },
    /// 分区里有可识别的内容，但没有可供该操作作用的文件系统。
    NoFilesystem { number: u32, what: String },
    /// 该内容类别不支持该操作。
    ContentUnsupported { what: String, why: String },
    /// 内容属性不能按请求写入：目标形态或请求形式不在能力内（拒绝，而非不支持）。
    PropertyRefused { why: String },
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
    /// 目标是块设备上的分区节点，写命令只接受整盘或镜像。
    PartitionNodeTarget { command: String, target: String },
    /// 目标容量装不下要新建的表。
    TargetTooSmall { why: String },
    /// 目标表区已有数据，覆盖需要显式旗标。
    TableOccupied { probe: String, regions: String },
    /// 保护 MBR 的 SizeInLBA 比两个合法取值都大，拒绝自动修复。
    PmbrInconsistent { why: String },
    /// 记录文件存在但无法解码。
    RecordUnreadable { path: String },
    /// 记录结构损坏：截断、校验不符或格式不认。
    RecordDamaged { why: String },
    /// 记录属于另一个目标。
    RecordForeign { why: String },
    /// 目标上有一条针对别的工作的未收尾记录。
    RecordConflict { path: String },
    /// 记录恢复不能按请求执行：外来记录、载荷不可回滚、或放弃会丢掉可回放区间。
    RecoveryRefused { why: String },
    /// 表编辑本身不能成立；原因来自纯核的编辑判据。
    TableEdit { why: String },
    /// LVM 链的只读查询失败：设备/报表读不出来，本次尚未写目标字节。
    LvmFailed { why: String },
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
            Self::MountNotMounted { .. } => {
                "pass the mount point of a mounted partition, or use the :N partition form"
            }
            Self::MountAmbiguous { .. } => {
                "unmount the stacked layers so the mount point has a single filesystem, then retry"
            }
            Self::MountNotPartition { .. } => {
                "use the :N form for a whole-disk or logical-volume target"
            }
            Self::MountNotBlockDevice { .. } | Self::MountUnmapped { .. } => {
                "use a single-device filesystem, or use the :N partition form"
            }
            Self::OnlineShrink { .. } => {
                "use resize to shrink the partition and filesystem together"
            }
            Self::TableDamaged { .. } => {
                "inspect the evidence with read-only commands first, and recover from a backup copy if necessary"
            }
            Self::GptRequired { .. } => {
                "use a target whose partition table is GPT, or drop the flag that needs it"
            }
            Self::RecordNotFound { .. } => {
                "list the records for this target and use one of the reported identifiers"
            }
            Self::RecordStoreMissing { .. } => {
                "let a writing command create the anchor directory first, or inspect the target on the host that owns it"
            }
            Self::RecordDirectoryUnresolved { .. } => {
                "give --alias NAME for a block device without a readable WWN, EUI or serial, or address an image"
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
            Self::NoFilesystem { .. } => {
                "create a filesystem in the partition first, or use a command that matches its content"
            }
            Self::ContentUnsupported { .. } => {
                "use an operation or external tool supported by this content kind"
            }
            Self::PropertyRefused { .. } => {
                "use the filesystem's own utility for this content, or change the request"
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
            Self::PartitionNodeTarget { .. } => {
                "point the command at the whole disk or the image, not at a partition node"
            }
            Self::TargetTooSmall { .. } => {
                "use a larger target, a smaller sector size, or the other table format"
            }
            Self::TableOccupied { .. } => {
                "rerun with --overwrite-data after confirming those bytes may be replaced"
            }
            Self::PmbrInconsistent { .. } => {
                "inspect the protective MBR on the owning host; automatic repair is refused"
            }
            Self::RecordUnreadable { .. } => {
                "inspect the record file; recover or remove it before writing again"
            }
            Self::RecordDamaged { .. } | Self::RecordForeign { .. } => {
                "keep the record file for manual inspection; nothing may write to this target until then"
            }
            Self::RecordConflict { .. } => {
                "finish or abandon the existing record with `diskedit undo`, `diskedit replay` or `diskedit release` before starting a different plan"
            }
            Self::RecoveryRefused { .. } => {
                "inspect the record with `diskedit records show`, then choose `diskedit replay` or `diskedit release`"
            }
            Self::TableEdit { .. } => {
                "inspect the current table and adjust the request so the edit can be placed"
            }
            Self::LvmFailed { .. } => "inspect the LVM state with pvs/vgs/lvs, then retry",
        }
    }

    /// 固定的一句话成因，供命令行报告使用。
    pub fn describe(&self) -> String {
        match self {
            Self::ConfirmationRequired => "this command writes and needs --yes".to_owned(),
            Self::OverwriteNotConfirmed => {
                "the target already holds data; overwriting needs the explicit flag".to_owned()
            }
            Self::IdentityUnprovable { why } => format!("target identity is unprovable: {why}"),
            Self::IdentityChanged => {
                "the target is not the object the plan was made for".to_owned()
            }
            Self::PlanDigestMismatch => {
                "the plan is not self-consistent: it is empty, repeats a step, or no longer matches \
                 its digest"
                    .to_owned()
            }
            Self::OccupancyUnknown { why } => format!("usage of the target is unknown: {why}"),
            Self::Occupied { evidence } => format!("the target is in use: {evidence}"),
            Self::FactUnknown { what } => format!("a required fact is unknown: {what}"),
            Self::GeometryUnknown { why } => format!("target geometry is unknown: {why}"),
            Self::TableMissing => "there is no usable partition table".to_owned(),
            Self::MountNotMounted { path } => format!("{path} is not a mount point"),
            Self::MountAmbiguous { path } => format!("{path} has more than one mount record"),
            Self::MountNotPartition { device } => format!("{device} is not a partition"),
            Self::MountNotBlockDevice { source } => {
                format!("{source} is not a single block device")
            }
            Self::MountUnmapped { reason } => format!("cannot map the mount to a device: {reason}"),
            Self::OnlineShrink { why } => format!("this form cannot shrink the filesystem: {why}"),
            Self::TableDamaged { why } => format!("the partition table is damaged: {why}"),
            Self::GptRequired { what } => what.clone(),
            Self::RecordNotFound { token } => format!("no recovery record has id {token}"),
            Self::RecordStoreMissing { path } => {
                format!("the anchor directory {path} cannot be opened")
            }
            Self::RecordDirectoryUnresolved { target } => {
                format!("{target} has no record directory — this target exposes no stable identity")
            }
            Self::PartitionMissing { number } => format!("partition {number} does not exist"),
            Self::PartitionOverlap { number } => {
                format!("partition {number} would overlap an existing partition")
            }
            Self::CapacityInsufficient { needed, available } => {
                format!("needed {needed} bytes but only {available} are available")
            }
            Self::FilesystemUnknown { number } => {
                format!("the content of partition {number} is not identified")
            }
            Self::NoFilesystem { number, what } => {
                format!("partition {number} holds {what}, which is not a filesystem to resize")
            }
            Self::ContentUnsupported { what, why } => format!("{what} is not supported: {why}"),
            Self::PropertyRefused { why } => why.clone(),
            Self::MinimumSizeUnknown { number, why } => {
                format!("the minimum size of partition {number} is unknown: {why}")
            }
            Self::BelowMinimumSize {
                number,
                minimum,
                requested,
            } => format!(
                "partition {number} cannot shrink below {minimum} bytes (requested {requested})"
            ),
            Self::ToolMissing { program } => format!("{program:?} is not available"),
            Self::PlatformCannotRunTools { why } => {
                format!("this platform cannot run external tools: {why}")
            }
            Self::ArgumentOutOfRange { what } => format!("argument out of range: {what}"),
            Self::FlagUnknown { why } => format!("unsupported flag: {why}"),
            Self::PartitionNodeTarget { command, target } => {
                format!("{command} does not write a partition node: {target}")
            }
            Self::TargetTooSmall { why } => format!("the target is too small: {why}"),
            Self::TableOccupied { probe, regions } => format!(
                "the table sectors already hold data (probe={probe}); regions {regions} would be overwritten"
            ),
            Self::PmbrInconsistent { why } => {
                format!("the protective MBR is inconsistent: {why}")
            }
            Self::RecordUnreadable { path } => {
                format!("the record file {path} cannot be decoded")
            }
            Self::RecordDamaged { why } | Self::RecordForeign { why } => why.clone(),
            Self::RecordConflict { path } => {
                format!("{path} holds an unfinished record for a different plan")
            }
            Self::RecoveryRefused { why } => {
                format!("the recovery record cannot be finished here: {why}")
            }
            Self::TableEdit { why } => format!("the table cannot take this change: {why}"),
            Self::LvmFailed { why } => format!("the LVM chain cannot proceed: {why}"),
        }
    }
}
