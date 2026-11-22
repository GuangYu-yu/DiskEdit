//! Native 放置：把载体步骤映射到 [`crate::execution`] 的单步实现。

use super::{
    CarrierStep, FsStep, ProbeRequest, ProbeReport, StepRun, TransferStep,
};
use crate::dev::WriteSource;
use crate::execution::{self, StepSink, TransferState, copy, fs, mkfs, relocate, set, swap, table};
use crate::outcome::RunOutcome;

pub(crate) fn execute(
    src: &mut WriteSource,
    step: CarrierStep<'_>,
    sector_size: u64,
    state: &mut TransferState,
    sink: &mut dyn StepSink,
) -> Result<StepRun, RunOutcome> {
    match step {
        CarrierStep::TableCommit(commit) => {
            table::execute(src, commit)?;
            Ok(StepRun::applied())
        }
        CarrierStep::Transfer(TransferStep::Relocate(reloc)) => {
            relocate::execute(src, *reloc, sector_size, state, sink)?;
            Ok(StepRun::applied())
        }
        CarrierStep::Transfer(TransferStep::Copy { step, chunk }) => {
            copy::execute(src, step, sector_size, chunk)?;
            Ok(StepRun::applied())
        }
        CarrierStep::Fs(FsStep::Resize(step)) => {
            // 未达成的后置条件不折进结论：由 `with_pending` 一并交成"停在原地"的待办
            Ok(StepRun::with_pending(fs::execute(src, step, sector_size, sink)?))
        }
        CarrierStep::Fs(FsStep::Create(step)) => {
            mkfs::execute(src, step, sector_size)?;
            Ok(StepRun::applied())
        }
        CarrierStep::Fs(FsStep::Set(step)) => {
            set::execute(src, step)?;
            Ok(StepRun::applied())
        }
        CarrierStep::Fs(FsStep::RebuildSwap(step)) => match swap::rebuild(src, step, sector_size, sink)? {
            None => Ok(StepRun::applied()),
            // 换区内容没重建：表项已在，内容仍是待办
            Some(item) => Ok(StepRun::with_pending(vec![item])),
        },
        CarrierStep::Fs(FsStep::HiddenSectors(reloc)) => {
            execution::step_hidden_sectors(src, reloc, sector_size, sink)?;
            Ok(StepRun::applied())
        }
    }
}

/// 探测帧的独立入口：结论以 [`ProbeReport`] 直接交出，不经过步骤结果的可选字段。
/// 计划步骤不含探测——[`WireStep`](super::protocol::WireStep) 没有探测变体，探测只由
/// worker 的 probe 帧发起，因此探测结论在类型上就不是"可能缺失"
pub(crate) fn execute_probe(
    src: &WriteSource,
    request: ProbeRequest,
    sector_size: u64,
) -> Result<ProbeReport, RunOutcome> {
    probe(src, request, sector_size)
}

/// 只读探测：按请求的落点识别内容类别。地址溢出与读失败都按错误交出
fn probe(
    src: &WriteSource,
    request: ProbeRequest,
    sector_size: u64,
) -> Result<ProbeReport, RunOutcome> {
    let base = request.first_lba.checked_mul(sector_size).ok_or_else(|| {
        RunOutcome::internal(format!(
            "partition {}: start LBA {} overflows the address space",
            request.part, request.first_lba
        ))
    })?;
    let len = request.sectors.checked_mul(sector_size).ok_or_else(|| {
        RunOutcome::internal(format!(
            "partition {}: {} sectors overflow the address space",
            request.part, request.sectors
        ))
    })?;
    let content = crate::fsid::identify(src, base, len)
        .map_err(|e| RunOutcome::partial(format!("partition {}: {e}", request.part)))?;
    Ok(ProbeReport { part: request.part, content })
}
