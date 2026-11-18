//! 数据搬移步骤：按块布局在源区间与目的区间之间逐块直读直写，块边界向 sink 报进度。
//!
//! 每块经写入口落盘并带步骤归属；回读按 durable 批次做：同步之后、推进游标之前
//! 回读整批并比对，不逐块回读。

use sha2::{Digest, Sha256};

use crate::dev::{RegionStamp, WriteSource};
use crate::outcome::{RunOutcome, WriteStep};
use crate::planner::chunk::{relocation_overlaps, spans, ChunkSpan};

use super::{StepSink, TransferState};

/// 一个搬移步骤的几何与块布局：分区号、源/目的起点、要搬的扇区数，
/// 请求的块字节数与首次写入前算定的有效块字节数、块数
#[derive(Clone, Copy)]
pub(crate) struct Relocate {
    pub part: u32,
    pub src_first: u64,
    pub dst_first: u64,
    pub sectors: u64,
    pub chunk_bytes: u64,
    pub chunk: u64,
    pub chunks: u64,
}

/// 非自重叠搬移攒批落盘的检查点阈值下限
const CHECKPOINT_FLOOR: u64 = 8 * 1024 * 1024;

pub(crate) fn execute(
    src: &mut WriteSource,
    step: Relocate,
    sector_size: u64,
    state: &mut TransferState,
    sink: &mut dyn StepSink,
) -> Result<(), RunOutcome> {
    let Relocate { part, src_first, dst_first, sectors, chunk_bytes, chunk, chunks: count } = step;
    let shift = dst_first.abs_diff(src_first);
    if state.chunks_done() > count {
        return Err(RunOutcome::partial(format!(
            "partition {part}: {} chunks are recorded as done but this step only has {count} — the transfer may be incomplete",
            state.chunks_done()
        )));
    }
    // 位移 0：源与目的重合，数据已在位，没有拷贝要做，故不比对目的前缀。
    // 块进度已满则必须先过目的前缀比对（见下），否则伪造的"已满"记录会直接跳到表步
    if shift == 0 {
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
    // 上界：步的两段字节区间都必须落在目标自身之内。越界的步是计划与盘上几何不符，
    // 不裁到盘尾（裁剪会写下与记录声明不同的字节数，游标随之前进）
    if src_base + total > src.size || dst_base + total > src.size {
        let detail = format!(
            "partition {part}: the relocation range ends at byte {} (target) / {} (source), past the \
             {}-byte target",
            dst_base + total,
            src_base + total,
            src.size
        );
        return Err(RunOutcome::refused(detail));
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
    // 记录里的块数只是声明：续跑先按同一执行顺序回读目的前缀、与记录里的摘要比对。
    // 不一致说明记录陈旧或伪造，按它跳过块拷贝不可逆，故一律停在这里
    let done = state.chunks_done();
    let mut hasher = Sha256::new();
    if done > 0 {
        for span in ordered.iter().take(done as usize) {
            let within = (span.first_lba - src_first) * sector_size;
            let len = span.len_lba * sector_size;
            let mut buf = vec![0u8; len as usize];
            let dst = dst_base + within;
            if let Err(e) = src.read_at(dst, &mut buf) {
                let message = format!(
                    "partition {part}: cannot read the target at byte {dst} to verify the recorded \
                     progress: {e} — the transfer may be incomplete"
                );
                return Err(RunOutcome::no_mutation(message));
            }
            hasher.update(&buf);
        }
        let seen: [u8; 32] = hasher.clone().finalize().into();
        // 摘要必须是这一段目的内容本身：零摘要（记录没给依据）与任何一份真实摘要都不等，
        // 同样落进拒绝——按无法验证的块数跳过拷贝不可逆
        if seen != state.prefix_digest() {
            let message = format!(
                "partition {part}: the target's copied prefix does not match the recovery record \
                 (recorded {done} chunk(s)) — the recorded progress cannot be trusted, so no further \
                 data was moved"
            );
            return Err(RunOutcome::refused(message));
        }
    }
    if done == count {
        return Ok(());
    }
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

    for span in ordered.iter().skip(state.chunks_done() as usize) {
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
        state.advance_chunk();
        // 摘要跟着 durable 前缀滚动：块的写入已返回成功，目的区间的内容就是 `buf`
        hasher.update(&buf);
        state.set_prefix_digest(hasher.clone().finalize().into());
        if overlap {
            if let Err(e) = src.sync_data() {
                return Err(RunOutcome::partial(format!(
                    "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
                )));
            }
            verify_batch(src, batch_start, &batch, part)?;
            batch.clear();
            crate::movepart::fault_after_data_sync(state.cursor());
            sink.transfer_checkpoint(src, state)
                .map_err(|e| checkpoint_failed(part, e))?;
            crate::movepart::fault_chunk(state.cursor());
        } else {
            crate::movepart::fault_chunk(state.cursor());
            pending += len;
            if pending >= flush_at {
                if let Err(e) = src.sync_data() {
                    return Err(RunOutcome::partial(format!(
                        "partition {part}: cannot flush the moved data: {e} — the transfer may be incomplete"
                    )));
                }
                verify_batch(src, batch_start, &batch, part)?;
                batch.clear();
                crate::movepart::fault_after_data_sync(state.cursor());
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
        crate::movepart::fault_after_data_sync(state.cursor());
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
