//! `CopyPartition` 步骤：把源分区逐块复制到新落点，新条目由随后的表步骤提交。
//!
//! 复制固定前向、禁止任何重叠，没有逐块检查点也不续跑；数据字节不入记录，写下第一
//! 个目标字节之前落不可回滚屏障。文件系统的 HiddenSectors 修正在表提交之前完成——提交
//! 之后落点被占用，重跑会被判据拒绝，修正就再没有机会补上。

use crate::dev::{RegionStamp, WriteSource};
use crate::fsid::{ContentKind, FsKind};
use crate::movepart::{chunk_io_error, fault_copy_before_commit, fix_ntfs_hidden_sectors, COPY_HINT};
use crate::outcome::{RunOutcome, WriteStep};

/// 一个复制步骤：字段与 [`crate::planner::PlanStep::CopyPartition`] 一一对应。
pub(crate) struct CopyPartition {
    pub part: u32,
    pub src_first: u64,
    pub dst_first: u64,
    pub sectors: u64,
}

impl CopyPartition {
    /// 载荷换算成字节区间：源偏移、目标偏移与总长度。
    fn byte_range(&self, sector_size: u64) -> Result<(u64, u64, u64), String> {
        let total = self.sectors.checked_mul(sector_size).ok_or_else(|| {
            format!(
                "partition {}: {} sectors of {sector_size} bytes overflow the address space",
                self.part, self.sectors
            )
        })?;
        let src_off = self.src_first.checked_mul(sector_size).ok_or_else(|| {
            format!(
                "partition {}: source start LBA {} overflows the address space",
                self.part, self.src_first
            )
        })?;
        let dst_off = self.dst_first.checked_mul(sector_size).ok_or_else(|| {
            format!(
                "partition {}: target start LBA {} overflows the address space",
                self.part, self.dst_first
            )
        })?;
        for (label, base) in [("source", src_off), ("target", dst_off)] {
            if base.checked_add(total).is_none() {
                return Err(format!(
                    "partition {}: the {label} byte range {base}+{total} overflows the address space",
                    self.part
                ));
            }
        }
        Ok((src_off, dst_off, total))
    }

    /// 首写前的载荷自洽判定：非零长度、区间不溢出、两端都在目标容量内、源与目标不重叠。
    fn check_shape(&self, disk_bytes: u64, sector_size: u64) -> Result<(), String> {
        if self.sectors == 0 {
            return Err(format!("partition {}: the copy step moves zero sectors", self.part));
        }
        let (src_off, dst_off, total) = self.byte_range(sector_size)?;
        if src_off + total > disk_bytes {
            return Err(format!(
                "partition {}: the source range ends at byte {} beyond the target's {disk_bytes} bytes",
                self.part,
                src_off + total
            ));
        }
        if dst_off + total > disk_bytes {
            return Err(format!(
                "partition {}: the target range ends at byte {} beyond the target's {disk_bytes} bytes",
                self.part,
                dst_off + total
            ));
        }
        let src_end = self.src_first + self.sectors;
        let dst_end = self.dst_first + self.sectors;
        if self.src_first < dst_end && self.dst_first < src_end {
            return Err(format!(
                "partition {}: the copy source range {}..{} overlaps its target range {}..{}",
                self.part,
                self.src_first,
                src_end - 1,
                self.dst_first,
                dst_end - 1
            ));
        }
        Ok(())
    }
}

/// 首次写入前的预扫描：载荷自洽。不自洽说明计划没经过装配，按内部错误上报；
/// 落点与重叠的请求级判定由装配之前的 `prepare_copy` 完成。
pub(crate) fn preflight(
    src: &WriteSource,
    step: &CopyPartition,
    sector_size: u64,
) -> Result<(), RunOutcome> {
    step.check_shape(src.size, sector_size).map_err(RunOutcome::internal)
}

/// 执行（写盘）：屏障 → 逐块读源、喂摘要、写目标并同步 → 核对源摘要 → NTFS HiddenSectors 修正。
///
/// 表提交不在这里：新条目由计划里的表步骤承担。两半之间中断时盘上是"数据已复制、
/// 条目未提交"，undo 因屏障拒绝回滚，重跑 copy 可以把这个副本补齐。
pub(crate) fn execute(
    src: &mut WriteSource,
    step: &CopyPartition,
    sector_size: u64,
    chunk_bytes: u64,
) -> Result<(), RunOutcome> {
    use sha2::{Digest, Sha256};

    let (src_off, dst_off, total) = step.byte_range(sector_size).map_err(RunOutcome::internal)?;
    // 记录里的源摘要是这次搬移的核对基准。首跑没有可比对的先前值，核对只能落在实际读到
    // 的字节上；缺摘要说明计划没有经过绑定，不能在写完之后才发现这件事
    let expected = match src.record_journal().and_then(|j| j.source()) {
        Some(source) => source.digest,
        // 没有基准就无法核对这次拷贝；本次是否写过目标字节由出口的归责改判
        None => {
            return Err(RunOutcome::internal(format!(
                "partition {}: the copy step carries no recorded source digest — the copy cannot be \
                 verified against the source",
                step.part
            )))
        }
    };
    // 屏障必须早于首个目标写入：数据字节不入记录，undo 见到它即永久拒绝回滚
    src.mark_non_reversible()
        .map_err(|e| RunOutcome::partial(e.record_failure("the recovery barrier")))?;
    let total_n = total.div_ceil(chunk_bytes) as usize;
    let mut hasher = Sha256::new();
    let mut pos = 0u64;
    while pos < total {
        let len = chunk_bytes.min(total - pos);
        let mut buf = vec![0u8; len as usize];
        src.read_at(src_off + pos, &mut buf).map_err(|e| {
            RunOutcome::partial(
                chunk_io_error(
                    e,
                    "reading source",
                    (src_off + pos) / sector_size,
                    len / sector_size,
                    (pos / chunk_bytes + 1) as usize,
                    total_n,
                    COPY_HINT,
                )
                .to_string(),
            )
        })?;
        hasher.update(&buf);
        // 目的区间没有可比的期望内容：写入只受写后回读约束
        let dst = dst_off + pos;
        src.write_guarded(dst, &buf, RegionStamp::unconstrained(dst, len), WriteStep::Relocate { part: step.part })
            .map_err(|e| {
                RunOutcome::partial(
                    chunk_io_error(
                        e.into_io_error(),
                        "writing target",
                        dst / sector_size,
                        len / sector_size,
                        (pos / chunk_bytes + 1) as usize,
                        total_n,
                        COPY_HINT,
                    )
                    .to_string(),
                )
            })?;
        src.sync_data().map_err(|e| RunOutcome::partial(e.to_string()))?;
        // 单块即一个 durable 批次：同步之后回读，落盘的字节必须原样可读
        src.read_back(dst, &buf).map_err(|e| {
            RunOutcome::partial(
                chunk_io_error(
                    e,
                    "reading back the written target",
                    dst / sector_size,
                    len / sector_size,
                    (pos / chunk_bytes + 1) as usize,
                    total_n,
                    COPY_HINT,
                )
                .to_string(),
            )
        })?;
        pos += len;
    }
    let copied: [u8; 32] = hasher.finalize().into();
    if copied != expected {
        return Err(RunOutcome::partial(format!(
            "partition {}: the bytes read from the source do not match the digest recorded for it — \
             the copy is not faithful to the recorded source; the destination was written and `undo` \
             does not roll it back",
            step.part
        )));
    }
    fault_copy_before_commit();
    // 副本的 boot sector 原样带来旧 HiddenSectors，按新起始位置修正必须先于表项提交：
    // 提交之后落点被占用，重跑会被重叠判据拒绝，副本会永久停在旧的偏移上
    if crate::fsid::identify(src, dst_off, total).map_err(|e| RunOutcome::partial(e.to_string()))?
        == ContentKind::Filesystem(FsKind::Ntfs)
    {
        fix_ntfs_hidden_sectors(src, step.dst_first, sector_size, step.part, &mut |m| println!("{m}"))
            .map_err(|e| RunOutcome::partial(e.to_string()))?;
    }
    Ok(())
}
