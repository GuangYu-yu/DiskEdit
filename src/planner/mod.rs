//! 计划的步骤类型与装配协议：装配层产出的步骤序列在这里收敛成 `Plan`，
//! 执行层只读它。
//!
//! 本层只持有值，不打开目标、不取锁、不做 I/O。

pub(crate) mod chunk;

use crate::cli::Capability;
use crate::domain::{ContentKind, SectorSize};
use crate::fsops::MkfsFs;
use crate::platform::CapabilityId;
use crate::table::TableStep;

/// 目标上分区表的可判定形态
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableObservation {
    /// 盘上没有任何可识别的分区表
    Absent,
    /// 表存在但结构不可用（结构损坏、条目越界等）：请求与现状不符，出口 3
    Unusable { reason: String },
    /// 解析成功的表；可用区由生产方按表的规则推导，本层不猜
    Present(TableFacts),
}

/// 解析后的一张表
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableFacts {
    pub(crate) entries: Vec<PartitionFact>,
    pub(crate) first_usable: u64,
    pub(crate) last_usable: u64,
}

/// 解析后的一个分区条目；只保留装配需要的几何
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartitionFact {
    pub(crate) number: u32,
    pub(crate) first_lba: u64,
    pub(crate) last_lba: u64,
}

impl PartitionFact {
    pub(crate) fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// 步骤的类型；执行顺序只由这些值表达
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanStep {
    /// 表写步骤：`TableStep` 在计划期装配好要写的字节与两端表戳
    WriteTable(TableStep),
    /// 同一分区原地改位置；`src_first != dst_first`
    ///
    /// `is_swap` 由装配点填入：为真的步骤在执行前按能力缺口拒绝。与
    /// [`crate::recovery::Relocation`] 的对应：本变体在计划中的出现顺序即
    /// `relocation.entries` 的顺序，同序第 i 个 `Relocate` 对应 `entries[i]`，
    /// `entry_index` 取该序号，`chunks_in_entry` 是该条目内的块进度
    Relocate { part: u32, src_first: u64, dst_first: u64, sectors: u64, is_swap: bool },
    /// 把源分区复制到新落点，源分区保留
    CopyPartition { part: u32, src_first: u64, dst_first: u64, sectors: u64 },
    /// 换区重建：表项已提交到新落点、内容可弃，按旧头里的 UUID/卷标重新 mkswap。
    ///
    /// 只出现在它自己那条 `Relocate { is_swap: true }` 与提交该条目的表步骤之后；
    /// 不改表、不动数据区。失败是"后置条件未达成"而不是本步没做成：旧签名仍在源位，
    /// 记录保留，重跑从本条目重试
    RebuildSwap { part: u32, src_first: u64, dst_first: u64, sectors: u64 },
    /// 把分区里的文件系统改写到分区尺寸；语义是扩到分区满。
    ///
    /// 几何按表的扇区口径：`first_lba` 是分区起点，`sectors` 是分区长度，
    /// `target_bytes` 是这段区间的字节数。`fstype` 由装配点填写；装配层不做
    /// FS 探测，产出的计划恒为 `None`，执行期按区间识别
    FsResize {
        part: u32,
        first_lba: u64,
        sectors: u64,
        target_bytes: u64,
        fstype: Option<ContentKind>,
    },
    /// 在分区上创建文件系统。`fstype` 取请求本身的类型：ext2/3/4 必须区分，不能用
    /// 识别域折叠后的 ext 家族。`label` 原样作为工具参数，`None` 即不传。几何按表的
    /// 扇区口径：`first_lba` 是分区起点，`sectors` 是分区长度
    FsCreate {
        part: u32,
        first_lba: u64,
        sectors: u64,
        fstype: MkfsFs,
        label: Option<String>,
    },
    /// 改写分区文件系统内部的属性。`attr` 表达要写什么：UUID 可以是具体文本，也可以是
    /// 交给工具生成的意图；`fstype` 取命令层 `identify` 的结论，执行期不探测
    FsSet { part: u32, attr: FsSetAttr, fstype: Option<ContentKind> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FsSetAttr {
    /// 卷标，原样作为工具参数
    Label(String),
    /// UUID：写具体文本，或由工具生成
    Uuid(UuidValue),
}

/// UUID 属性的两种意图。能接受具体值的 FS 在装配期解析成文本；只能自生成的 FS（ntfs）
/// 把生成动作留给工具，具体值在那类目标上无处落实
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UuidValue {
    Explicit(String),
    Generate,
}

/// 一次操作的步骤序列；扇区大小是步骤长度换算成字节的依据
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    steps: Vec<PlanStep>,
    sector_size: SectorSize,
}

impl Plan {
    fn new(steps: Vec<PlanStep>, sector_size: SectorSize) -> Self {
        Self { steps, sector_size }
    }

    pub(crate) fn steps(&self) -> &[PlanStep] {
        &self.steps
    }

    pub(crate) fn sector_size(&self) -> SectorSize {
        self.sector_size
    }
}

/// 已装配的计划；字段私有，构造点是各 `assembled_*` 入口
#[derive(Debug)]
pub(crate) struct ConfirmedPlan {
    plan: Plan,
}

impl ConfirmedPlan {
    pub(crate) fn plan(&self) -> &Plan {
        &self.plan
    }

    /// 只有一条已装配表步骤的计划。确认门在解析期已由 `--yes` 判定（如 `delete`），
    /// 装配层在计划期装配完成后用它交给执行层。
    pub(crate) fn assembled_table_step(step: TableStep, sector_size: SectorSize) -> Self {
        Self { plan: Plan::new(vec![PlanStep::WriteTable(step)], sector_size) }
    }

    /// 只有一条 `FsResize` 步骤的计划。几何按表的扇区口径给出，`target_bytes` 由本构造点
    /// 从 `sectors × sector_size` 算出，调用方不另给字节数——区间长度与目标字节数因此
    /// 不会在装配处出现两种说法。区间的扇区数换算成字节溢出时不产出计划。
    pub(crate) fn assembled_fs_resize(
        part: u32,
        first_lba: u64,
        sectors: u64,
        fstype: Option<ContentKind>,
        sector_size: SectorSize,
    ) -> Option<Self> {
        let target_bytes = sectors.checked_mul(sector_size.get())?;
        let plan = Plan::new(
            vec![PlanStep::FsResize { part, first_lba, sectors, target_bytes, fstype }],
            sector_size,
        );
        Some(Self { plan })
    }

    /// 只有一条 `FsCreate` 步骤的计划。几何按表的扇区口径给出；`fstype` 与 `label`
    /// 原样带入执行步骤，不在装配处折算成识别域的类型或补默认卷标。
    pub(crate) fn assembled_fs_create(
        part: u32,
        first_lba: u64,
        sectors: u64,
        fstype: MkfsFs,
        label: Option<String>,
        sector_size: SectorSize,
    ) -> Self {
        let plan = Plan::new(
            vec![PlanStep::FsCreate { part, first_lba, sectors, fstype, label }],
            sector_size,
        );
        Self { plan }
    }

    /// 只有一条 `FsSet` 步骤的计划。值与类型由装配点给出，两者都已解析成最终形态：
    /// 本构造点不补默认值，也不生成 UUID。
    pub(crate) fn assembled_fs_set(
        part: u32,
        attr: FsSetAttr,
        fstype: Option<ContentKind>,
        sector_size: SectorSize,
    ) -> Self {
        let plan = Plan::new(vec![PlanStep::FsSet { part, attr, fstype }], sector_size);
        Self { plan }
    }

    /// 一条搬移作业的整份已装配计划：搬移与其表提交成对出现，换区条目在提交之后多一步
    /// 重建，末尾可再跟一个扩容表步骤。
    ///
    /// 形状由 [`move_plan_shape`] 校验：`Relocate` 后必跟表步骤，`RebuildSwap` 前必有
    /// 表步骤，表步骤两端表戳齐全。其它排列在这个入口上表达不出来。
    pub(crate) fn assembled_move_plan(steps: Vec<PlanStep>, sector_size: SectorSize) -> Option<Self> {
        if !move_plan_shape(&steps) {
            return None;
        }
        Some(Self { plan: Plan::new(steps, sector_size) })
    }

    /// `copy` 的已装配计划：一条复制步骤在前，提交副本条目的表步骤在后。复制步骤不是
    /// `CopyPartition`、表步骤两端表戳不全时返回 `None`，不产出计划。
    pub(crate) fn assembled_copy(
        copy: PlanStep,
        table: TableStep,
        sector_size: SectorSize,
    ) -> Option<Self> {
        if !matches!(copy, PlanStep::CopyPartition { .. })
            || table.expect_before.is_none()
            || table.expect_after.is_none()
        {
            return None;
        }
        let plan = Plan::new(vec![copy, PlanStep::WriteTable(table)], sector_size);
        Some(Self { plan })
    }
}

/// 搬移作业计划的形状判据：搬移与其表提交成对，换区重建紧跟在自己的表提交之后，
/// 其余类型不在这条作业里出现。表步骤两端表戳齐全才算已装配。
///
/// 判据写成"每一条步骤都合法且配对成立"，而不是枚举合法序列：配对本身是局部的，
/// 扩容表步骤因此可以出现在任何一对之后而无需在这里再列一种排列
fn move_plan_shape(steps: &[PlanStep]) -> bool {
    if steps.is_empty() {
        return false;
    }
    steps.iter().enumerate().all(|(i, step)| match step {
        PlanStep::Relocate { .. } => matches!(steps.get(i + 1), Some(PlanStep::WriteTable(_))),
        PlanStep::RebuildSwap { .. } => i > 0 && matches!(steps.get(i - 1), Some(PlanStep::WriteTable(_))),
        PlanStep::WriteTable(table) => table.expect_before.is_some() && table.expect_after.is_some(),
        PlanStep::CopyPartition { .. } | PlanStep::FsResize { .. } | PlanStep::FsCreate { .. } | PlanStep::FsSet { .. } => false,
    })
}

/// 命令声明的能力类别到平台原语的映射；帮助生成的能力矩阵读同一份映射。
/// 空声明按漏声明处理，不会得到放行项。
pub(crate) fn needs_for(capabilities: &[Capability]) -> Vec<CapabilityId> {
    let mut needs = Vec::new();
    for cap in capabilities {
        match cap {
            Capability::ReadOnly => needs.push(CapabilityId::ReadTarget),
            Capability::TargetData => {
                needs.push(CapabilityId::WriteTable);
                needs.push(CapabilityId::SectorIo);
            }
            Capability::CompanionFile => {
                needs.push(CapabilityId::AtomicReplace);
                needs.push(CapabilityId::DirPersist);
            }
            Capability::ExternalTool => needs.push(CapabilityId::ExternalTool),
            Capability::DeviceState => needs.push(CapabilityId::KernelResync),
        }
    }
    needs
}
