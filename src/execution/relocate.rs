//! 数据搬移步骤：按块布局在源区间与目的区间之间逐块直读直写，块边界向 sink 报进度。

use crate::dev::WriteSource;
use crate::outcome::RunOutcome;
use crate::planner::chunk::{move_chunk_layout, relocation_overlaps};

use super::{StepSink, TransferState};

/// 一个搬移步骤的几何：分区号、源/目的起点与要搬的扇区数
#[derive(Clone, Copy)]
pub(crate) struct Relocate {
    pub part: u32,
    pub src_first: u64,
    pub dst_first: u64,
    pub sectors: u64,
}

/// 非自重叠搬移攒批落盘的检查点阈值下限
const CHECKPOINT_FLOOR: u64 = 8 * 1024 * 1024;

pub(crate) fn execute(
    src: &mut WriteSource,
    step: Relocate,
    sector_size: u64,
    chunk_bytes: u64,
    state: &mut TransferState,
    sink: &mut dyn StepSink,
) -> Result<(), RunOutcome> {
    let Relocate { part, src_first, dst_first, sectors } = step;
    let shift = dst_first.abs_diff(src_first);
    let layout = if sectors == 0 {
        None
    } else {
        move_chunk_layout(sectors, sector_size, shift, sectors, chunk_bytes)
    };
    // 同一布局已在首次写入前的预扫描里算过；这里仍算不出说明计划与执行输入不一致，
    // 此刻前面的条目可能已经写过目标字节，按"可能已改变"报告
    let Some((chunk, count)) = layout else {
        return Err(RunOutcome::partial(format!(
            "partition {part}: the relocation block layout cannot be computed — the transfer may be incomplete"
        )));
    };
    if state.chunks_in_entry > count {
        return Err(RunOutcome::partial(format!(
            "partition {part}: {} chunks are recorded as done but this step only has {count} — the transfer may be incomplete",
            state.chunks_in_entry
        )));
    }
    // 位移 0 没有拷贝要做；块进度已满说明数据搬完、只差表步
    if shift == 0 || state.chunks_in_entry == count {
        return Ok(());
    }

    let src_base = src_first.checked_mul(sector_size).ok_or_else(|| {
        RunOutcome::partial(format!(
            "partition {part}: the source offset overflows the address space — the transfer may be incomplete"
        ))
    })?;
    let dst_base = dst_first.checked_mul(sector_size).ok_or_else(|| {
        RunOutcome::partial(format!(
            "partition {part}: the target offset overflows the address space — the transfer may be incomplete"
        ))
    })?;
    let total = sectors.checked_mul(sector_size).ok_or_else(|| {
        RunOutcome::partial(format!(
            "partition {part}: the relocation length overflows the address space — the transfer may be incomplete"
        ))
    })?;
    if src_base.checked_add(total).is_none() || dst_base.checked_add(total).is_none() {
        return Err(RunOutcome::partial(format!(
            "partition {part}: the relocation byte range overflows the address space — the transfer may be incomplete"
        )));
    }

    // 序列方向由位移符号决定：目的区间在源之后（正位移）时从尾往头逐块搬，否则从头往尾。
    // `chunks_in_entry` 是这条序列的前缀长度，两个方向共用下面同一段迭代。
    let from_tail = dst_first > src_first;
    let index_at = |n: u64| if from_tail { count - 1 - n } else { n };
    let overlap = relocation_overlaps(shift, sectors);
    if overlap && chunk < chunk_bytes {
        sink.note(&format!(
            "partition {part}: self-overlapping move, chunk size reduced to {chunk} bytes to keep redo reads intact"
        ));
    }
    let flush_at = chunk_bytes.max(CHECKPOINT_FLOOR);
    // 已有块进度或更早的条目已提交，都表示目标字节已被写过：后续失败不再属于"未改动"
    let mut written = state.wrote_target();
    let mut pending = 0u64;

    for n in state.chunks_in_entry..count {
        let i = index_at(n);
        let within = i * chunk;
        let len = chunk.min(total - within);
        let mut buf = vec![0u8; len as usize];
        if let Err(e) = src.read_at(src_base + within, &mut buf) {
            let off = src_base + within;
            return Err(if written {
                RunOutcome::partial(format!(
                    "partition {part}: cannot read the source at byte {off}: {e} — the transfer may be incomplete"
                ))
            } else {
                RunOutcome::no_mutation(format!(
                    "partition {part}: cannot read the source at byte {off}: {e} — nothing was written"
                ))
            });
        }
        written = true;
        if let Err(e) = src.write_data_at(dst_base + within, &buf) {
            return Err(RunOutcome::partial(format!(
                "partition {part}: cannot write the target at byte {}: {e} — the transfer may be incomplete",
                dst_base + within
            )));
        }
        state.chunks_in_entry += 1;
        state.transfer_cursor += 1;
        if overlap {
            if let Err(e) = src.sync_data() {
                return Err(RunOutcome::partial(format!(
                    "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
                )));
            }
            crate::movepart::fault_after_data_sync(state.transfer_cursor);
            sink.transfer_checkpoint(src, state)
                .map_err(|e| checkpoint_failed(part, e))?;
            crate::movepart::fault_chunk(state.transfer_cursor);
        } else {
            pending += len;
            if pending >= flush_at {
                if let Err(e) = src.sync_data() {
                    return Err(RunOutcome::partial(format!(
                        "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
                    )));
                }
                sink.transfer_checkpoint(src, state)
                    .map_err(|e| checkpoint_failed(part, e))?;
                pending = 0;
            }
        }
    }
    if !overlap && pending > 0 {
        if let Err(e) = src.sync_data() {
            return Err(RunOutcome::partial(format!(
                "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
            )));
        }
        sink.transfer_checkpoint(src, state)
            .map_err(|e| checkpoint_failed(part, e))?;
    }
    Ok(())
}

/// 回调发生在数据已经写下去之后：无论 sink 报的是哪一类，结论都是搬移可能不完整
fn checkpoint_failed(part: u32, e: RunOutcome) -> RunOutcome {
    match e {
        RunOutcome::Partial { .. } => e,
        other => RunOutcome::partial(format!(
            "partition {part}: the progress checkpoint was not persisted: {other} — the transfer may be incomplete"
        )),
    }
}
