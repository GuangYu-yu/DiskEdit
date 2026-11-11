//! 数据搬移步骤：按块布局在源区间与目的区间之间逐块直读直写，块边界向 sink 报进度。
//!
//! 每块经写入口落盘并带步骤归属；回读按 durable 批次做：同步之后、推进游标之前
//! 回读整批并比对，不逐块回读。

use crate::dev::{RegionStamp, WriteSource};
use crate::outcome::{RunOutcome, WriteStep};
use crate::planner::chunk::{move_chunk_layout, relocation_overlaps, spans, ChunkSpan};

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
    // 块边界由 [`spans`] 唯一给出；`chunks_in_entry` 是这条序列的前缀长度
    let from_tail = dst_first > src_first;
    let mut ordered: Vec<ChunkSpan> = spans(src_first, sectors, chunk / sector_size).collect();
    if from_tail {
        ordered.reverse();
    }
    // 边界与块数同源：不一致说明块大小不是扇区的整数倍，宁可停在这里也不按错位搬
    if ordered.len() as u64 != count {
        return Err(RunOutcome::partial(format!(
            "partition {part}: the relocation block layout is inconsistent ({count} chunks expected, {} produced) — \
             the transfer may be incomplete",
            ordered.len()
        )));
    }
    let overlap = relocation_overlaps(shift, sectors);
    if overlap && chunk < chunk_bytes {
        sink.note(&format!(
            "partition {part}: self-overlapping move, chunk size reduced to {chunk} bytes to keep redo reads intact"
        ));
    }
    let flush_at = chunk_bytes.max(CHECKPOINT_FLOOR);
    let mut pending = 0u64;
    // 当前 durable 批次的起点与已写字节：回读用它们，批次跟随同步点推进
    let mut batch_start = dst_base;
    let mut batch: Vec<u8> = Vec::new();

    for span in ordered.iter().skip(state.chunks_in_entry as usize) {
        let within = (span.first_lba - src_first) * sector_size;
        let len = span.len_lba * sector_size;
        let mut buf = vec![0u8; len as usize];
        if let Err(e) = src.read_at(src_base + within, &mut buf) {
            let off = src_base + within;
            return Err(RunOutcome::no_mutation(format!(
                "partition {part}: cannot read the source at byte {off}: {e} — the transfer may be incomplete"
            )));
        }
        let dst = dst_base + within;
        // 目的区间没有可比的期望内容：只受写后回读约束
        if let Err(e) =
            src.write_guarded(dst, &buf, RegionStamp::unconstrained(dst, len), WriteStep::Relocate { part })
        {
            return Err(RunOutcome::partial(format!(
                "partition {part}: cannot write the target at byte {dst}: {e} — the transfer may be incomplete"
            )));
        }
        append_batch(&mut batch, &mut batch_start, dst, &buf);
        state.chunks_in_entry += 1;
        state.transfer_cursor += 1;
        if overlap {
            if let Err(e) = src.sync_data() {
                return Err(RunOutcome::partial(format!(
                    "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
                )));
            }
            verify_batch(src, batch_start, &batch, part)?;
            batch.clear();
            crate::movepart::fault_after_data_sync(state.transfer_cursor);
            sink.transfer_checkpoint(src, state)
                .map_err(|e| checkpoint_failed(part, e))?;
            crate::movepart::fault_chunk(state.transfer_cursor);
        } else {
            crate::movepart::fault_chunk(state.transfer_cursor);
            pending += len;
            if pending >= flush_at {
                if let Err(e) = src.sync_data() {
                    return Err(RunOutcome::partial(format!(
                        "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
                    )));
                }
                verify_batch(src, batch_start, &batch, part)?;
                batch.clear();
                crate::movepart::fault_after_data_sync(state.transfer_cursor);
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
        verify_batch(src, batch_start, &batch, part)?;
        sink.transfer_checkpoint(src, state)
            .map_err(|e| checkpoint_failed(part, e))?;
    }
    Ok(())
}

/// 同步之后回读一个 durable 批次：批次内字节必须在盘上原样可读，否则不推进游标
fn verify_batch(src: &WriteSource, start: u64, batch: &[u8], part: u32) -> Result<(), RunOutcome> {
    src.read_back(start, batch).map_err(|e| {
        RunOutcome::partial(format!(
            "partition {part}: the moved data did not read back as written: {e} — the transfer may be incomplete"
        ))
    })
}

/// 把刚写的一块拼进当前批次。块序可能自尾向头（正位移），批次因此按地址升序排，
/// 回读区间才是一段连续字节
fn append_batch(batch: &mut Vec<u8>, batch_start: &mut u64, dst: u64, buf: &[u8]) {
    if batch.is_empty() {
        *batch_start = dst;
        batch.extend_from_slice(buf);
    } else if dst < *batch_start {
        let mut grown = Vec::with_capacity(batch.len() + buf.len());
        grown.extend_from_slice(buf);
        grown.extend_from_slice(batch);
        *batch = grown;
        *batch_start = dst;
    } else {
        batch.extend_from_slice(buf);
    }
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
