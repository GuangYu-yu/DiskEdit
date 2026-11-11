//! 表写步骤：计划期一次算出「要写的字节」与两端表戳，执行期只按 `regions` 读回比对，
//! 不再读盘推算内容。
//!
//! 写入目标（`writes`）与读回比对范围（`regions`）是两个概念，比对范围默认等于写入范围：
//! `Gpt` 写入为备份项数组、备份头、主项数组、主头四段，调用方给出保护性 MBR 时追加 LBA0，
//! 比对范围取四段与 LBA0、LBA1、末 LBA 的并集；`Mbr` 常规只写 LBA0，`new --table mbr` 另清
//! LBA1、末 LBA 与三段之外的 GPT 签名位置，比对范围是 LBA0、LBA1、末 LBA 与那些签名段。
//!
//! `expect_before` 由调用方在读出表内容之后立刻读成 [`TableSnapshot`] 并传入，装配期不再
//! 读盘；`expect_after` 由写入字节与快照原文算出。

use super::*;

/// 表步骤对表的改动类别；影响面只由它表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableWriteKind {
    /// 新建整张表
    Replace,
    /// 增加一个条目
    Add,
    /// 删除一个条目
    Remove,
    /// 表长度增加
    Grow,
    /// 表长度减少
    Shrink,
    /// 条目位置改变，长度不变
    Repartition,
    /// 修复动作：重写双头或保护 MBR，不改分区布局
    Repair,
}

/// 计划期算好的新 GPT 表内容：新头、新条目数组、备份头落点与该表自身的逻辑块大小。
/// 纯值，不带盘面；字节布局与三扇区摘要都按这里的 `ss` 换算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GptContent {
    /// 该表自身的逻辑块大小：条目 LBA 与末 LBA 的换算单位（可与容器 ss 不同）
    pub(crate) ss: u64,
    pub(crate) header: RawHeader,
    pub(crate) entries: Vec<GPTPartitionEntry>,
    /// 备份头的落点（本表 ss 口径）；备份条目数组位于它之前一个跨度
    pub(crate) last_lba: u64,
}

/// 一段写入：目标区间 + 计划期算好的字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepWrite {
    pub(crate) region: TableRegion,
    pub(crate) bytes: Vec<u8>,
}

/// 一个表写步骤。`regions` 是写前/写后比对的区间集合，`writes` 是实际写入的段，每个写入
/// 段都必须落在 `regions` 之内。`expect_before` 由调用方读定后传入，`expect_after` 由
/// 写入字节与快照原文算出，执行期不重算。
///
/// 两端表戳为 `None` 只表示该步骤尚未装配：执行层一律拒绝、不写盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableStep {
    pub(crate) kind: TableWriteKind,
    pub(crate) regions: Vec<TableRegion>,
    /// 与 `regions` 等长同序的写前期望：首次写盘之前逐段核对盘上表区用
    pub(crate) expect_regions: Vec<RegionStamp>,
    pub(crate) writes: Vec<StepWrite>,
    /// 与 `writes` 等长同序的写前期望：每条写入段自己的区间摘要，取自同一份快照
    pub(crate) before: Vec<RegionStamp>,
    pub(crate) expect_before: Option<TableStamp>,
    pub(crate) expect_after: Option<TableStamp>,
}

/// GPT 的比对区间：LBA0、LBA1、主项数组、备份项数组、末 LBA，按字节偏移升序。纯计算；
/// 调用方据此在读出表内容之后立刻取快照。几何与落点判据与写入段同源。
pub(crate) fn gpt_compare_regions(src: &FileSource, gpt: &GptContent) -> Result<Vec<TableRegion>, RunOutcome> {
    let ss = gpt.ss;
    let last_lba = gpt.last_lba;
    let geom = EntryArrayGeometry::new(
        ss,
        gpt.header.size_of_partition_entry,
        gpt.header.number_of_partition_entries,
        MAX_ARRAY_BYTES,
    )
    .map_err(|e| RunOutcome::refused(e.to_string()))?;
    let span = geom.lba_span();
    let backup_array_lba = gpt
        .last_lba
        .checked_sub(span)
        .ok_or_else(|| RunOutcome::refused("disk too small to hold GPT entry array"))?;
    let min_sectors = 2 * span + 4;
    if gpt.last_lba.checked_add(1).is_none_or(|sectors| sectors < min_sectors) {
        return Err(RunOutcome::refused(gpt_too_small(min_sectors, ss)));
    }
    let file_last_lba = container_last_lba(src, ss);
    if gpt.last_lba > file_last_lba {
        return Err(RunOutcome::refused(format!(
            "GPT end LBA {} is beyond the container end {file_last_lba} — refusing to write the table",
            gpt.last_lba
        )));
    }
    let mut regions = vec![TableRegion { offset: 0, len: ss }];
    for (lba, sectors) in [(1, 1), (2, span), (backup_array_lba, span), (last_lba, 1)] {
        let region = TableRegion::lba(lba, sectors, ss).map_err(|e| RunOutcome::refused(e.to_string()))?;
        regions.push(region);
    }
    Ok(regions)
}

/// MBR 的比对区间：LBA0、LBA1、末 LBA 三扇区，按字节偏移升序；容器在某个位置不足一整
/// 扇区时按实际可读字节计入（与三扇区摘要的取法一致，同一 LBA 只计一次）。
pub(crate) fn mbr_compare_regions(src: &FileSource) -> Result<Vec<TableRegion>, RunOutcome> {
    let ss = src.sector_size;
    let mut regions = Vec::with_capacity(3);
    for lba in [0u64, 1, container_last_lba(src, ss)] {
        let offset = lba
            .checked_mul(ss)
            .ok_or_else(|| RunOutcome::refused("table region offset overflows"))?;
        if regions.iter().any(|r: &TableRegion| r.offset == offset) {
            continue;
        }
        regions.push(TableRegion { offset, len: src.size.saturating_sub(offset).min(ss) });
    }
    Ok(regions)
}

/// `new --table mbr` 的比对区间：LBA0、LBA1、末 LBA 三扇区，加上三段之外需要清零的
/// GPT 签名位置（各 8 字节），按字节偏移升序。三段之内的签名随整段清零一起覆盖，不再
/// 单列，比对区间因此互不重叠。
pub(crate) fn new_mbr_regions(src: &FileSource) -> Result<Vec<TableRegion>, RunOutcome> {
    let ss = src.sector_size;
    let mut regions = vec![TableRegion { offset: 0, len: ss }];
    for lba in [1u64, container_last_lba(src, ss)] {
        let region = TableRegion::lba(lba, 1, ss).map_err(|e| RunOutcome::refused(e.to_string()))?;
        if regions.iter().any(|r| r.offset == region.offset) {
            continue;
        }
        regions.push(region);
    }
    let sig_len = GPT_SIGNATURE.len() as u64;
    for off in gpt_signature_sites(src).map_err(RunOutcome::no_mutation_io)? {
        if regions
            .iter()
            .any(|r| off < r.offset.saturating_add(r.len) && r.offset < off.saturating_add(sig_len))
        {
            continue;
        }
        regions.push(TableRegion { offset: off, len: sig_len });
    }
    regions.sort_by_key(|r| r.offset);
    Ok(regions)
}

impl TableStep {
    /// GPT 写入段按写入口的崩溃安全序列排列：备份项数组、备份头、主项数组、主头。LBA0
    /// 保护性 MBR 在最后一段：内容由调用方用 [`protective_mbr_content`] 算好传入，`None`
    /// 即不写 LBA0（盘上那份已经规范）。两段头各含 CRC 字段所在扇区。字节与几何判据复用
    /// 写入口同一套序列化与校验，LBA 换算用内容自带的表 ss。
    pub(crate) fn gpt(
        src: &FileSource,
        kind: TableWriteKind,
        gpt: &GptContent,
        protective_mbr: Option<&[u8]>,
        snapshot: TableSnapshot,
    ) -> Result<Self, RunOutcome> {
        let ss = gpt.ss;
        let header = &gpt.header;
        let entries = &gpt.entries;
        let last_lba = gpt.last_lba;
        let geom = EntryArrayGeometry::new(
            ss,
            header.size_of_partition_entry,
            header.number_of_partition_entries,
            MAX_ARRAY_BYTES,
        )
        .map_err(|e| RunOutcome::refused(e.to_string()))?;
        let given = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        if given != geom.entry_count {
            return Err(RunOutcome::refused(format!(
                "{given} partition entries given for a header declaring {} — refusing to write the table",
                geom.entry_count
            )));
        }
        let span = geom.lba_span();
        let backup_array_lba = last_lba.checked_sub(span).ok_or_else(|| {
            RunOutcome::refused("disk too small to hold GPT entry array")
        })?;
        let (array_bytes, array_crc) = serialize_array(entries, &geom);
        let (primary, backup) = canonical_headers(header, last_lba, backup_array_lba);
        let primary_header =
            serialize_header(&primary, array_crc, ss).map_err(|e| RunOutcome::refused(e.to_string()))?;
        let backup_header =
            serialize_header(&backup, array_crc, ss).map_err(|e| RunOutcome::refused(e.to_string()))?;

        // 用**将要写出的**两份头过一遍解析侧同一套几何与条目校验。产出表不合法是工具自身
        // 的不变量破坏，发生在写目标字节之前，落 1；请求层面的几何/长度/
        // 序列化拒绝仍由上面各处落 3
        let file_last_lba = container_last_lba(src, ss);
        let backup_view = RawHeader { primary_lba: 1, backup_lba: last_lba, ..backup.clone() };
        for (h, view, what) in
            [(&primary, GptCopyKind::Primary, "primary"), (&backup_view, GptCopyKind::Backup, "backup")]
        {
            let state = validate_geometry(h, &geom, file_last_lba, view).map_err(|e| {
                RunOutcome::internal(format!(
                    "the {what} GPT header this write would produce is not self-consistent: {e}"
                ))
            })?;
            if state != GptState::Valid {
                return Err(RunOutcome::internal(format!(
                    "the {what} GPT header this write would produce does not match the container end"
                )));
            }
        }
        validate_entries(entries, &primary).map_err(|e| {
            RunOutcome::internal(format!("the partition entries this write would produce are invalid: {e}"))
        })?;

        let regions = gpt_compare_regions(src, gpt)?;
        let mut writes = vec![
            StepWrite {
                region: TableRegion::lba(backup_array_lba, span, ss)
                    .map_err(|e| RunOutcome::refused(e.to_string()))?,
                bytes: array_bytes.clone(),
            },
            StepWrite {
                region: TableRegion::lba(last_lba, 1, ss).map_err(|e| RunOutcome::refused(e.to_string()))?,
                bytes: backup_header,
            },
            StepWrite {
                region: TableRegion::lba(2, span, ss).map_err(|e| RunOutcome::refused(e.to_string()))?,
                bytes: array_bytes,
            },
            StepWrite {
                region: TableRegion::lba(1, 1, ss).map_err(|e| RunOutcome::refused(e.to_string()))?,
                bytes: primary_header,
            },
        ];
        // LBA0 只在调用方给出算好的整扇区内容时写：`None` 表示盘上那份已经规范
        if let Some(lba0) = protective_mbr {
            let region = TableRegion { offset: 0, len: ss };
            writes.push(StepWrite { region, bytes: lba0.to_vec() });
        }
        Self::assemble(kind, regions, writes, snapshot)
    }

    /// 只重写 LBA0 保护 MBR：比对区间仍覆盖该表四段与 LBA0，写入段只有 LBA0。
    ///
    /// 修复动作使用：四段必须已与 `gpt` 一致，写前比对会拦住不一致的现场；
    /// 写后比对的其余四段取快照原文，因此它们的不变也被证明。
    pub(crate) fn protective_mbr(
        src: &FileSource,
        gpt: &GptContent,
        lba0: &[u8],
        snapshot: TableSnapshot,
    ) -> Result<Self, RunOutcome> {
        let ss = gpt.ss;
        if lba0.len() as u64 != ss {
            return Err(RunOutcome::refused(format!(
                "protective MBR image is {} byte(s) for a {ss}-byte sector — refusing to write the table",
                lba0.len()
            )));
        }
        let regions = gpt_compare_regions(src, gpt)?;
        let writes = vec![StepWrite {
            region: TableRegion { offset: 0, len: ss },
            bytes: lba0.to_vec(),
        }];
        Self::assemble(TableWriteKind::Repair, regions, writes, snapshot)
    }

    /// MBR 常规写入：只有 LBA0。`lba0` 是计划期算好的整扇区新内容。
    pub(crate) fn mbr(
        src: &FileSource,
        kind: TableWriteKind,
        lba0: &[u8],
        snapshot: TableSnapshot,
    ) -> Result<Self, RunOutcome> {
        let ss = src.sector_size;
        if lba0.len() as u64 != ss {
            return Err(RunOutcome::refused(format!(
                "MBR image is {} byte(s) for a {ss}-byte sector — refusing to write the table",
                lba0.len()
            )));
        }
        let regions = mbr_compare_regions(src)?;
        let writes = vec![StepWrite { region: TableRegion { offset: 0, len: ss }, bytes: lba0.to_vec() }];
        Self::assemble(kind, regions, writes, snapshot)
    }

    /// `new --table mbr`：写 LBA0（保留 BootCode、置签名、清空 4 条记录），整扇区清零
    /// LBA1 与末 LBA，并把三段之外的 GPT 签名位置各清 8 字节。写入顺序是签名段、LBA1、
    /// 末 LBA、LBA0：不留"新 MBR 已落盘、旧 GPT 头还在 LBA1"的中间态。LBA0 原文取自快照，
    /// 装配期不读盘；落在 LBA0 内的候选签名位置只在缓冲里清零，不另开写段。
    pub(crate) fn new_mbr(src: &FileSource, snapshot: TableSnapshot) -> Result<Self, RunOutcome> {
        let ss = src.sector_size;
        if src.size < 3 * ss {
            return Err(RunOutcome::refused(format!(
                "target too small for an MBR (needs ≥3 sectors at {ss}-byte sector size)"
            )));
        }
        let regions = snapshot.regions.clone();
        let Some(pos) = regions.iter().position(|r| r.offset == 0) else {
            return Err(RunOutcome::internal("the table snapshot is empty — LBA0 is missing"));
        };
        if regions[pos].len != ss {
            return Err(RunOutcome::internal(format!(
                "the table snapshot's LBA0 is {} byte(s) for a {ss}-byte sector",
                regions[pos].len
            )));
        }
        let mut lba0 = snapshot.bytes[pos].clone();
        lba0[446..510].fill(0);
        lba0[510] = 0x55;
        lba0[511] = 0xAA;
        // 候选扇区大小的 GPT 主头签名若落在 LBA0 内，随这次整块回写一并清零：留着它这块
        // 新 msdos 盘会被本工具判成 GptDamaged，而工具内没有修复入口
        let sig_len = GPT_SIGNATURE.len();
        for at in candidate_sector_sizes(ss) {
            let at = at as usize;
            if at + sig_len > lba0.len() {
                continue;
            }
            if lba0[at..at + sig_len] == *GPT_SIGNATURE {
                lba0[at..at + sig_len].fill(0);
            }
        }
        let last_off = container_last_lba(src, ss)
            .checked_mul(ss)
            .ok_or_else(|| RunOutcome::refused("table region offset overflows"))?;
        let mut writes = Vec::with_capacity(regions.len());
        // 三段之外的签名段先写，三段按 LBA1 → 末 LBA → LBA0 的次序
        for r in &regions {
            if r.offset == 0 || r.offset == ss || r.offset == last_off {
                continue;
            }
            writes.push(StepWrite { region: *r, bytes: vec![0u8; r.len as usize] });
        }
        for off in [ss, last_off] {
            let Some(region) = regions.iter().find(|r| r.offset == off) else {
                continue;
            };
            let len = usize::try_from(region.len)
                .map_err(|_| RunOutcome::refused("sector size does not fit the host address space"))?;
            writes.push(StepWrite { region: *region, bytes: vec![0u8; len] });
        }
        writes.push(StepWrite { region: regions[pos], bytes: lba0 });
        Self::assemble(TableWriteKind::Replace, regions, writes, snapshot)
    }

    fn assemble(
        kind: TableWriteKind,
        regions: Vec<TableRegion>,
        writes: Vec<StepWrite>,
        snapshot: TableSnapshot,
    ) -> Result<Self, RunOutcome> {
        if snapshot.bytes.len() != regions.len() {
            return Err(RunOutcome::internal(
                "the table snapshot does not cover the compare regions — the step was not assembled from one read",
            ));
        }
        if writes.iter().any(|w| {
            !regions.contains(&w.region) || w.bytes.len() as u64 != w.region.len
        }) {
            return Err(RunOutcome::internal(
                "a write target is outside the compare range or carries the wrong length — the step was not assembled",
            ));
        }
        // 写后摘要：写入段取计划期算好的字节，未写段取快照原文（它们在写入前后不变）
        let mut parts: Vec<&[u8]> = Vec::with_capacity(regions.len());
        for (i, region) in regions.iter().enumerate() {
            match writes.iter().find(|w| w.region == *region) {
                Some(w) => parts.push(&w.bytes),
                None => parts.push(&snapshot.bytes[i]),
            }
        }
        // 写前期望按写入段的实际顺序排：每条写入段取它在快照里的那段原文
        let mut before = Vec::with_capacity(writes.len());
        for w in &writes {
            let i = regions.iter().position(|r| *r == w.region).ok_or_else(|| {
                RunOutcome::internal("a write target is not in the compare regions — the step was not assembled")
            })?;
            before.push(RegionStamp::of_bytes(w.region.offset, &snapshot.bytes[i]));
        }
        let expect_after = table_stamp_of(&parts);
        let expect_regions =
            regions.iter().zip(&snapshot.bytes).map(|(r, b)| RegionStamp::of_bytes(r.offset, b)).collect();
        Ok(Self {
            kind,
            regions,
            expect_regions,
            writes,
            before,
            expect_before: Some(snapshot.stamp),
            expect_after: Some(expect_after),
        })
    }
}
