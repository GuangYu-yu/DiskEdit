//! 步骤执行：把一种步骤变成声明区间内的字节写入。
//!
//! 表格式、复制与搬移直接经写通道落盘；文件系统操作在写前闸口通过后，由
//! [`filesystem`] 在分区设备视图上运行外部工具，并回读目标字节核对结果。

pub mod boot;
pub mod copy;
pub mod entries;
pub mod filesystem;
pub mod table;

use std::path::Path;

use diskedit_core::region::Region;

use super::relocate;
use super::token::Recorded;
use super::{ExecError, WriteChannel};
use crate::app::decide::step::StepKind;
use crate::app::observe::SetProperty;
use crate::platform::Platform;

/// 执行一步所需的宿主资源。
#[derive(Clone, Copy)]
pub struct Host<'a> {
    pub platform: &'a dyn Platform,
    pub target: &'a Path,
}

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
        return Err(ExecError::InvalidPlan(
            "sector size cannot be zero".to_owned(),
        ));
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
        }
        | StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. } => Ok(()),
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords { .. } => Err(ExecError::InvalidPlan(
            "this step does not write target bytes".to_owned(),
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
    host: &Host<'_>,
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
        StepKind::CopyPartition {
            source,
            start_lba,
            sectors,
            chunk,
        } => copy::apply(*source, *start_lba, *sectors, *chunk, channel, token, ctx),
        StepKind::MovePartition {
            chunk, ntfs_hidden, ..
        } => {
            let (source, destination) = match regions {
                [source, destination] => (*source, *destination),
                _ => {
                    return Err(ExecError::InvalidPlan(
                        "move step must declare both old and new regions".to_owned(),
                    ));
                }
            };
            let binding = relocate::Binding::new(source, destination, *chunk)?;
            relocate::apply(binding, channel, token, relocate::RelocateFaults::default())?;
            if *ntfs_hidden {
                boot::fix_ntfs_hidden(channel, token, destination, ctx.sector)?;
            }
            Ok(())
        }
        StepKind::SetProperty {
            property: SetProperty::Label | SetProperty::Uuid,
            ..
        }
        | StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. } => {
            filesystem::apply(kind, regions, channel, token, ctx, host)
        }
        StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords { .. } => Err(ExecError::InvalidPlan(
            "no target-write implementation for this step".to_owned(),
        )),
    }
}
