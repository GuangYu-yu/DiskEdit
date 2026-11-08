//! 表区摘要（`TableStamp`）：三扇区写前复核与表步骤的区间数组摘要共用同一实现。
//! 表步骤的写前摘要由调用方读成 [`TableSnapshot`] 后传入，装配期不再读盘。

use super::*;
use sha2::{Digest, Sha256};

/// 表内容的稳定摘要：32 字节 sha256 全值，逐字节相等比较，不比较前缀。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableStamp([u8; 32]);

impl std::fmt::Debug for TableStamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// 表区里的一段：字节偏移与长度。摘要与读回比对都以它为输入单元。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableRegion {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

impl TableRegion {
    /// 按逻辑块标定一段：从 `lba` 起 `sectors` 个整块。
    pub(crate) fn lba(lba: u64, sectors: u64, ss: u64) -> io::Result<Self> {
        let offset = lba
            .checked_mul(ss)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "table region offset overflows"))?;
        let len = sectors
            .checked_mul(ss)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "table region length overflows"))?;
        Ok(Self { offset, len })
    }
}

impl TableStamp {
    /// 首 LBA（0）、LBA1、容器末 LBA 三处按 LBA 升序拼接后算 sha256。同一 LBA
    /// 只计一次；容器在该位置不足一整扇区时按实际可读字节计入。输入是盘面字节，
    /// 不含扇区大小、表类型、路径与时间。
    ///
    /// `ss` 是本次写所依据的逻辑块大小：GPT 取表自身的 ss（4Kn 表放在 512 字节镜像里时
    /// 与容器 ss 不等，按容器换算会读到别的块），msdos 取容器 ss。
    fn head_tail(src: &FileSource, ss: u64) -> io::Result<Self> {
        let mut lbas = [0u64, 1, container_last_lba(src, ss)];
        lbas.sort_unstable();
        let mut hasher = Sha256::new();
        let mut prev: Option<u64> = None;
        for lba in lbas {
            if prev == Some(lba) {
                continue;
            }
            prev = Some(lba);
            let off = lba
                .checked_mul(ss)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "table sector offset overflows"))?;
            let len = src.size.saturating_sub(off).min(ss) as usize;
            let mut buf = vec![0u8; len];
            src.read_at(off, &mut buf)?;
            hasher.update(&buf);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        Ok(Self(out))
    }

    /// 区间数组摘要：按数组顺序逐段读入、段内按字节升序拼接。输入只有盘面字节，
    /// 不含区间之外的位置、扇区大小与路径。
    fn over(src: &FileSource, regions: &[TableRegion]) -> io::Result<Self> {
        Ok(Self::over_snapshot(src, regions)?.stamp)
    }

    /// 区间数组摘要 + 逐段原文：一次读取同时给出写前摘要与快照，写后摘要的未写区间
    /// 直接取自这份原文，不必再读盘。
    fn over_snapshot(src: &FileSource, regions: &[TableRegion]) -> io::Result<TableSnapshot> {
        let mut hasher = Sha256::new();
        let mut bytes = Vec::with_capacity(regions.len());
        for r in regions {
            let len = usize::try_from(r.len)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "table region too large to hash"))?;
            let mut buf = vec![0u8; len];
            src.read_at(r.offset, &mut buf)?;
            hasher.update(&buf);
            bytes.push(buf);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        Ok(TableSnapshot { stamp: Self(out), regions: regions.to_vec(), bytes })
    }

    /// 字节切片数组摘要：与 [`Self::over`] 同一拼接规则，输入取自计划期算好的写入字节。
    fn of_parts(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for p in parts {
            hasher.update(p);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        Self(out)
    }
}

/// 计划期一次读定的比对区间快照：写前摘要、区间本身与逐段原文（`bytes` 与
/// `regions` 等长同序）。未写区间的写后期望字节直接取自 `bytes`。
#[derive(Clone)]
pub(crate) struct TableSnapshot {
    pub(crate) stamp: TableStamp,
    pub(crate) regions: Vec<TableRegion>,
    pub(crate) bytes: Vec<Vec<u8>>,
}

impl TableSnapshot {
    /// 首 LBA、LBA1、容器末 LBA 三段是否全零。快照缺任一段即按"非全零"处理：
    /// 无法证明这些位置是空的，就不许把它当成空白盘放行。
    pub(crate) fn head_tail_all_zero(&self, ss: u64, last_lba: u64) -> bool {
        [0u64, 1, last_lba].iter().all(|&lba| {
            let offset = lba.saturating_mul(ss);
            self.regions
                .iter()
                .zip(&self.bytes)
                .find(|(r, _)| r.offset == offset)
                .is_some_and(|(_, b)| b.iter().all(|&x| x == 0))
        })
    }
}

/// 记录三扇区摘要。写入路径在首笔写盘之前调用；从这次读定到首笔写盘之间不得有写。
/// `ss` 的取法见 [`TableStamp::head_tail`]。
pub(crate) fn table_stamp(src: &FileSource, ss: u64) -> Result<TableStamp, RunOutcome> {
    TableStamp::head_tail(src, ss).map_err(|e| stamp_io_failure(src, e))
}

/// 写前复核：重读三扇区并与 `expected` 比对。不等即不写：记录已落盘 → 5，
/// 尚未落盘（首跑）→ 3；重读 I/O 失败按同一分界落 5 / 4。
pub(crate) fn verify_table_stamp(src: &FileSource, expected: TableStamp, ss: u64) -> Result<(), RunOutcome> {
    let now = TableStamp::head_tail(src, ss).map_err(|e| stamp_io_failure(src, e))?;
    if now != expected {
        return Err(if record_persisted(src) {
            RunOutcome::partial(format!(
                "the table changed on disk before the write (stamp {expected:?} → {now:?}) — the on-disk state may have changed"
            ))
        } else {
            RunOutcome::refused(format!(
                "the table changed on disk after it was read (stamp {expected:?} → {now:?}) — re-run the command"
            ))
        });
    }
    Ok(())
}

/// 计划期写前快照：按 `regions` 读一次，同时得到写前摘要与各区间原文。取点由调用方定——
/// 必须在读出表内容之后立刻取，装配层不再读盘。
pub(crate) fn table_snapshot(src: &FileSource, regions: &[TableRegion]) -> Result<TableSnapshot, RunOutcome> {
    TableStamp::over_snapshot(src, regions).map_err(RunOutcome::no_mutation_io)
}

/// 计划期算出的写入字节的摘要：与 [`TableStamp::over`] 同一拼接规则，执行期不重算。
pub(crate) fn table_stamp_of(parts: &[&[u8]]) -> TableStamp {
    TableStamp::of_parts(parts)
}

/// 表步骤写前复核：按 `regions` 重读并与 `expected` 比对。不等即不执行——
/// 整体落到 5（InspectOnly）；重读失败时按本计划是否已写过目标字节分码：未写过按
/// 4（NoMutation）报，写过按 5 报。
///
/// `wrote_target` 由执行器从执行进度状态读出，表示这一表步之前本计划是否已经写过目标字节。
pub(crate) fn verify_regions_before(
    src: &FileSource,
    regions: &[TableRegion],
    expected: TableStamp,
    wrote_target: bool,
) -> Result<(), RunOutcome> {
    let now = TableStamp::over(src, regions).map_err(|e| {
        if wrote_target {
            RunOutcome::partial(format!(
                "re-reading the table regions before the write failed: {e} — earlier steps may already have written the target"
            ))
        } else {
            RunOutcome::no_mutation_io(e)
        }
    })?;
    if now != expected {
        return Err(RunOutcome::partial(format!(
            "the table changed on disk before the write (stamp {expected:?} → {now:?}) — the on-disk state may have changed"
        )));
    }
    Ok(())
}

/// 表区现状是否仍等于 `expected`。只读比较，不含退出码决策：首写前的调用点落 3，
/// 表步骤执行期的调用点落 5，两者的时点不同。重读失败按 4 报（一笔未写）。
pub(crate) fn regions_match(
    src: &FileSource,
    regions: &[TableRegion],
    expected: TableStamp,
) -> Result<bool, RunOutcome> {
    let now = TableStamp::over(src, regions).map_err(RunOutcome::no_mutation_io)?;
    Ok(now == expected)
}

/// 表步骤写后复核：按 `regions` 重读并与计划期由写入字节算出的 `expected` 比对。
/// 不等即不推进 progress——落 5；重读失败同样落 5（字节已写过）。
pub(crate) fn verify_regions_after(
    src: &FileSource,
    regions: &[TableRegion],
    expected: TableStamp,
) -> Result<(), RunOutcome> {
    let now = TableStamp::over(src, regions).map_err(|e| {
        RunOutcome::partial(format!(
            "re-reading the table regions after the write failed: {e} — the write may not have landed"
        ))
    })?;
    if now != expected {
        return Err(RunOutcome::partial(format!(
            "the table on disk differs from what was written (stamp {expected:?} → {now:?}) — progress is not advanced"
        )));
    }
    Ok(())
}

fn record_persisted(src: &FileSource) -> bool {
    src.record_journal.as_ref().is_some_and(crate::recovery::RecordJournal::is_persisted)
}

fn stamp_io_failure(src: &FileSource, e: io::Error) -> RunOutcome {
    if record_persisted(src) {
        RunOutcome::partial(format!("re-reading the table stamp failed: {e} — the on-disk state may have changed"))
    } else {
        RunOutcome::no_mutation_io(e)
    }
}
