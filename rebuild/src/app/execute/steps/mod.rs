//! 步骤执行：把一种步骤变成声明区间内的字节写入。
//!
//! 表格式与复制在本模块落地；需要分区设备路径的文件系统操作、在线扩缩与搬移
//! 由执行侧的其余子模块承担，未接入时在写前闸口返回 `Unsupported`，不落任何
//! 目标字节。

pub mod copy;
pub mod entries;
pub mod table;

use diskedit_core::region::Region;

use super::relocate;
use super::token::Recorded;
use super::{ExecError, WriteChannel};
use crate::app::decide::step::StepKind;
use crate::app::observe::SetProperty;

/// 执行一步所需的几何事实。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context<'a> {
    /// 计划摘要，用于派生稳定的 GUID。
    pub plan_digest: &'a [u8; 32],
    /// 逻辑扇区字节数。
    pub sector: u64,
}

/// 该步骤的写入是否只落在分区表区。
///
/// 表区原像必须携带原始字节以便回滚；数据区步骤不带原像字节，回滚时不动数据区。
pub fn writes_table(kind: &StepKind) -> bool {
    matches!(
        kind,
        StepKind::WriteTable { .. }
            | StepKind::AddPartition { .. }
            | StepKind::DeletePartition { .. }
            | StepKind::ResizePartition { .. }
            | StepKind::SetProperty {
                property: SetProperty::Name | SetProperty::Flag | SetProperty::Type,
                ..
            }
    )
}

/// 写前闸口：判断该步骤能否在本机执行；失败时不登记原像、不写目标。
pub fn precheck(kind: &StepKind, ctx: &Context<'_>) -> Result<(), ExecError> {
    if ctx.sector == 0 {
        return Err(ExecError::InvalidPlan("扇区尺寸不能为 0".to_owned()));
    }
    match kind {
        StepKind::WriteTable { .. }
        | StepKind::AddPartition { .. }
        | StepKind::DeletePartition { .. }
        | StepKind::ResizePartition { .. }
        | StepKind::CopyPartition { .. }
        | StepKind::MovePartition { .. } => Ok(()),
        StepKind::SetProperty {
            property: SetProperty::Name | SetProperty::Flag | SetProperty::Type,
            ..
        } => Ok(()),
        StepKind::SetProperty {
            property: SetProperty::Label | SetProperty::Uuid,
            ..
        } => Err(ExecError::Unsupported(
            "文件系统属性写入需要分区设备路径".to_owned(),
        )),
        StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. } => Err(ExecError::Unsupported(
            "文件系统操作需要分区设备路径".to_owned(),
        )),
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords => Err(ExecError::InvalidPlan(
            "该步骤不属于写目标字节的步骤".to_owned(),
        )),
    }
}

/// 执行一个已通过闸口的写目标步骤。
pub fn apply(
    kind: &StepKind,
    regions: &[Region],
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ctx: &Context<'_>,
) -> Result<(), ExecError> {
    match kind {
        StepKind::WriteTable { .. }
        | StepKind::AddPartition { .. }
        | StepKind::DeletePartition { .. }
        | StepKind::ResizePartition { .. }
        | StepKind::SetProperty {
            property: SetProperty::Name | SetProperty::Flag | SetProperty::Type,
            ..
        } => table::apply(kind, channel, token, ctx),
        StepKind::CopyPartition { source, target } => {
            copy::apply(*source, *target, channel, token, ctx)
        }
        StepKind::MovePartition { .. } => {
            let (source, destination) = match regions {
                [source, destination] => (*source, *destination),
                _ => {
                    return Err(ExecError::InvalidPlan(
                        "搬移步骤必须声明旧区间与新区间".to_owned(),
                    ));
                }
            };
            let binding = relocate::Binding::new(source, destination, relocate::CHUNK_BYTES)?;
            relocate::apply(binding, channel, token, relocate::RelocateFaults::default())?;
            relocate::verify(binding, channel)
        }
        StepKind::SetProperty {
            property: SetProperty::Label | SetProperty::Uuid,
            ..
        }
        | StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. } => {
            Err(ExecError::Unsupported("该步骤尚无可用的执行器".to_owned()))
        }
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords => Err(ExecError::InvalidPlan(
            "该步骤没有写目标字节的实现".to_owned(),
        )),
    }
}
