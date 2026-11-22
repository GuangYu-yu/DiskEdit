//! GPT 的崩溃安全写入：序列化、双头规范化与提交。

use std::mem::offset_of;

use super::*;

pub(crate) fn encode_name(entry: &GPTPartitionEntry) -> [u8; ENTRY_NAME_LEN] {
    let mut out = [0u8; ENTRY_NAME_LEN];
    let s = entry.partition_name.as_str();
    let units: Vec<u16> = s.encode_utf16().take(36).collect();
    // 第 36 码元为高代理时其低代理落在截断界外，回退 35 个码元保住完整代理对
    let n = if units.len() == 36 && (0xD800..=0xDBFF).contains(&units[35]) { 35 } else { units.len() };
    for (i, u) in units[..n].iter().enumerate() {
        out[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes()); // audit-allow: index-in-bounds
    }
    out
}

/// 条目序列化：128 字节规范布局（UEFI 2.10 §5.3.3 GPT Partition Entry Array，Table 5.6）
pub(crate) fn serialize_entry(e: &GPTPartitionEntry) -> [u8; ENTRY_FIXED_BYTES] {
    let mut b = [0u8; ENTRY_FIXED_BYTES];
    let ty = std::mem::offset_of!(GptEntryLayout, partition_type_guid);
    let uniq = std::mem::offset_of!(GptEntryLayout, unique_partition_guid);
    b[ty..uniq].copy_from_slice(&e.partition_type_guid);
    b[uniq..offset_of!(GptEntryLayout, starting_lba)].copy_from_slice(&e.unique_partition_guid);
    wr_u64(&mut b, offset_of!(GptEntryLayout, starting_lba), e.starting_lba);
    wr_u64(&mut b, offset_of!(GptEntryLayout, ending_lba), e.ending_lba);
    wr_u64(&mut b, offset_of!(GptEntryLayout, attributes), e.attribute_bits);
    b[ENTRY_NAME_OFFSET..ENTRY_NAME_OFFSET + ENTRY_NAME_LEN].copy_from_slice(&encode_name(e)); // audit-allow: index-in-bounds
    b
}

/// 头序列化：HeaderSize 字节有效 + 补零到扇区；CRC 对 HeaderSize 字节
/// （CRC 字段置 0）计算（UEFI 2.10 §5.3.2 Table 5.5：HeaderCRC32 覆盖 HeaderSize 字节，
/// HeaderSize ≥ 92 且 ≤ 逻辑块大小）。HeaderSize 之后到扇区末尾为保留区，恒写零
pub(crate) fn serialize_header(h: &RawHeader, array_crc: u32, ss: u64) -> io::Result<Vec<u8>> {
    if h.header_size < GPT_MIN_HEADER_SIZE || h.header_size as u64 > ss {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GPT HeaderSize {} out of range {GPT_MIN_HEADER_SIZE}..={ss}", h.header_size),
        ));
    }
    let ss_len = crate::support::host_usize(ss).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "the sector size exceeds the host address space")
    })?;
    let mut b = vec![0u8; ss_len];
    b[..GPT_SIGNATURE.len()].copy_from_slice(GPT_SIGNATURE);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, revision), h.revision);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, header_size), h.header_size);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, header_crc32), 0); // CRC 占位
    wr_u64(&mut b, offset_of!(GptHeaderLayout, my_lba), h.primary_lba);
    wr_u64(&mut b, offset_of!(GptHeaderLayout, alternate_lba), h.backup_lba);
    wr_u64(&mut b, offset_of!(GptHeaderLayout, first_usable_lba), h.first_usable_lba);
    wr_u64(&mut b, offset_of!(GptHeaderLayout, last_usable_lba), h.last_usable_lba);
    let guid = offset_of!(GptHeaderLayout, disk_guid);
    let entry_lba = offset_of!(GptHeaderLayout, partition_entry_lba);
    b[guid..entry_lba].copy_from_slice(&h.disk_guid);
    wr_u64(&mut b, entry_lba, h.partition_entry_lba);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, number_of_partition_entries), h.number_of_partition_entries);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, size_of_partition_entry), h.size_of_partition_entry);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, partition_entry_array_crc32), array_crc);
    let header_len = crate::support::host_usize(h.header_size).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "the GPT header size exceeds the host address space")
    })?;
    let crc = crc32(&b[..header_len]);
    wr_u32(&mut b, offset_of!(GptHeaderLayout, header_crc32), crc);
    Ok(b)
}

/// 条目数组序列化（含补零到扇区边界），返回 (字节, 数组 CRC)。
/// 几何由 [`EntryArrayGeometry`] 单点判定（es > 128 时条目尾部保留区保持零；
/// 不合规的几何在构造点即报错，此处不再自证）
pub fn serialize_array(
    entries: &[GPTPartitionEntry],
    geom: &EntryArrayGeometry,
) -> io::Result<(Vec<u8>, u32)> {
    let span = crate::support::host_usize(geom.span_bytes())
        .map_err(|_| invalid_geom("the GPT entry array span exceeds the host address space"))?;
    let mut b = vec![0u8; span];
    let count = crate::support::host_usize(geom.entry_count)
        .map_err(|_| invalid_geom("the GPT entry count exceeds the host address space"))?;
    let entry_size = crate::support::host_usize(geom.entry_size)
        .map_err(|_| invalid_geom("the GPT entry size exceeds the host address space"))?;
    for (i, e) in entries.iter().enumerate().take(count) {
        let off = i * entry_size; // audit-allow: index-in-bounds
        b[off..off + ENTRY_FIXED_BYTES].copy_from_slice(&serialize_entry(e)); // audit-allow: index-in-bounds
    }
    let byte_len = crate::support::host_usize(geom.byte_len())
        .map_err(|_| invalid_geom("the GPT entry byte length exceeds the host address space"))?;
    let crc = crc32(&b[..byte_len]);
    Ok((b, crc))
}

/// 重建规范化主/备头（写入路径：无论读到的是哪份副本，输出总为规范位置）
/// - 主头：MyLBA=1, Alt=last_lba, 数组=2
/// - 备头：MyLBA=last_lba, Alt=1, 数组=backup_array_lba（= last_lba − span）
pub(crate) fn canonical_headers(h: &RawHeader, last_lba: u64, backup_array_lba: u64) -> (RawHeader, RawHeader) {
    let primary = RawHeader {
        revision: h.revision,
        primary_lba: PRIMARY_HEADER_LBA,
        backup_lba: last_lba,
        first_usable_lba: h.first_usable_lba,
        last_usable_lba: h.last_usable_lba,
        disk_guid: h.disk_guid,
        partition_entry_lba: PRIMARY_ARRAY_LBA,
        number_of_partition_entries: h.number_of_partition_entries,
        size_of_partition_entry: h.size_of_partition_entry,
        header_size: h.header_size,
    };
    let backup = RawHeader {
        primary_lba: last_lba,
        backup_lba: PRIMARY_HEADER_LBA,
        first_usable_lba: h.first_usable_lba,
        last_usable_lba: h.last_usable_lba,
        disk_guid: h.disk_guid,
        partition_entry_lba: backup_array_lba,
        ..primary.clone()
    };
    (primary, backup)
}

/// 测试用提交入口：把已构造好的表交给**生产**同一条链路——装配成表步骤
/// （[`crate::assembly::assemble_table_step`]），再由 [`crate::execution::table::execute`]
/// 逐段做写前比对、逐段落盘、写后复核。
///
/// 这里刻意不再保留第二份四段写入序列：本文件曾经自己写四段并读回，而生产走
/// `execution::table`，测试跑的是另一份实现，"测试全绿"证明不了生产那份的顺序与复核。
/// 现在测试的可信度来自复用生产实现，而不是复制它。
#[cfg(test)]
pub(crate) fn commit_gpt(src: &mut WriteSource, g: &RawGpt, last_lba: u64) -> Result<(), RunOutcome> {
    commit_content(
        src,
        &GptContent { ss: g.ss, header: g.header.clone(), entries: g.entries.clone(), last_lba },
    )
}

/// 同上，内容按各分量给出：拒绝条件与生产装配完全同一套，调用方只换入参形状。
#[cfg(test)]
pub(crate) fn commit_table(
    src: &mut WriteSource,
    ss: u64,
    header: &RawHeader,
    entries: &[GPTPartitionEntry],
    last_lba: u64,
) -> Result<(), RunOutcome> {
    commit_content(
        src,
        &GptContent { ss, header: header.clone(), entries: entries.to_vec(), last_lba },
    )
}

/// 装配 + 执行：测试与生产之间唯一的表提交实现，两处差异只剩 `kind` 的标签。
#[cfg(test)]
fn commit_content(src: &mut WriteSource, content: &GptContent) -> Result<(), RunOutcome> {
    let table = crate::assembly::TableContent::Gpt {
        kind: TableWriteKind::Replace,
        content,
        protective_mbr: None,
    };
    let snapshot = crate::assembly::take_table_snapshot(&*src, &table)?;
    let step = crate::assembly::assemble_table_step(&*src, table, snapshot)?.into_step();
    crate::execution::table::execute(src, &step)
}

/// 规范 SizeInLBA = 逻辑块数 − 1；逻辑块数超出 32 位表示范围时按规范饱和写 0xFFFFFFFF
/// （UEFI 2.10 §5.2.3，LBA 字段以 logical block 计而非恒定 512 字节）。
/// "写入什么值"（[`ensure_protective_mbr`]）与"读到的值算不算规范"
/// （[`pmbr_size_state`]）共用这一处，两侧不各写一遍同一个算式
pub(crate) fn size_in_lba(total_sectors: u64) -> u32 {
    let last = last_sector_of(total_sectors);
    if last > u32::MAX as u64 {
        u32::MAX
    } else {
        last as u32
    }
}

/// 保护 MBR 的新扇区内容：保留 `0..446` 的引导代码，重写槽位 1、`446..510` 余下槽位、
/// 块内保留区与 `0x55AA` 签名。
pub(crate) fn protective_mbr_image(lba0: &mut [u8], total_sectors: u64) {
    let want_size = size_in_lba(total_sectors);
    lba0[MBR_ENTRY_AREA].fill(0);
    lba0[MBR_SECTOR_BYTES..].fill(0);
    let rec = &mut lba0[mbr_entry_range(0)];
    let start_at = offset_of!(MbrEntryLayout, starting_lba);
    let boot = offset_of!(MbrEntryLayout, boot_indicator);
    let first_chs = offset_of!(MbrEntryLayout, start_chs);
    let os_at = offset_of!(MbrEntryLayout, os_type);
    let last_chs = offset_of!(MbrEntryLayout, end_chs);
    rec[boot] = 0x00; // BootIndicator
    rec[first_chs..os_at].copy_from_slice(&[0x00, 0x02, 0x00]); // StartCHS 惯例值
    rec[os_at] = PROT_MBR_TYPE;
    rec[last_chs..start_at].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // EndCHS
    wr_u32(rec, start_at, 1);
    wr_u32(rec, offset_of!(MbrEntryLayout, size_in_lba), want_size);
    write_mbr_signature(lba0);
}

/// 保护 MBR 需要规范化时给出整扇区的新内容，已合规时返回 `None`（一字不写）。
/// 纯计算：只读 LBA0，不写盘。
///
/// 判据覆盖下方重写会改动的**每一个**字段：签名、槽位 2-4、块内保留区，以及槽位 1 的
/// BootIndicator、两个 CHS、类型、起点与 SizeInLBA（UEFI 2.10 §5.2.3 Table 5.4）。
/// 少查其中任何一项，都会把"其余字段非规范"的记录当作已合规。
///
/// `ss` 是**该表自身**的逻辑块大小（不是容器的 `src.sector_size`）：两者不等时
/// （4Kn 表放在 512 字节镜像里）按容器换算会整体差 8 倍。
pub(crate) fn protective_mbr_content(
    src: &FileSource,
    ss: u64,
) -> Result<Option<Vec<u8>>, RunOutcome> {
    // 扇区大小由调用方以裸数传入：先过唯一校验，零值与非 2 的幂在这里就落成拒绝。
    // 否则下面的除法与按扇区切片会分别变成除零与越界
    let ss = crate::domain::SectorSize::try_new(ss).map_err(|e| RunOutcome::refused(e.to_string()))?;
    let ss = ss.get();
    // 零扇区盘的 read_at(0) 只会撞出 UnexpectedEof，把"盘没有扇区"这件事
    // 说成一次 I/O 意外：先于任何读盘把几何前提判掉
    let total_sectors = src.size / ss;
    if total_sectors == 0 {
        return Err(RunOutcome::refused("disk has zero sectors"));
    }
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(|e| RunOutcome::no_mutation(e.to_string()))?;
    Ok(protective_mbr_content_from(&mut lba0, total_sectors))
}

/// [`protective_mbr_content`] 的纯计算部分：判定与重写只看传入的 LBA0 原文，不读盘。
/// 装配期传入写前快照里的那一扇区，决策依据与写基线因此同源
pub(crate) fn protective_mbr_content_from(lba0: &mut [u8], total_sectors: u64) -> Option<Vec<u8>> {
    // LBA0 比一个 MBR 扇区还短：这里没有可判定的保护 MBR，也没有可重写的字段
    if lba0.len() < MBR_SECTOR_BYTES {
        return None;
    }
    let want_size = size_in_lba(total_sectors);
    let reserved_zero = lba0[MBR_SECTOR_BYTES..].iter().all(|&b| b == 0);
    if has_mbr_signature(lba0) && reserved_zero {
        let rec = &lba0[mbr_entry_range(0)]; // 槽位 1（与下方写入位置一致）
        // 与 pmbr_shape_valid 同口径：槽位 2-4 必须全零，否则 hybrid MBR 残留
        // 会使 load_gpt 拒读 GPT（形状前置不满足），盘型判定就此失真
        if lba0[mbr_entry_range(1).start..MBR_SIGNATURE_AT].iter().all(|&b| b == 0)
            && rec[offset_of!(MbrEntryLayout, os_type)] == PROT_MBR_TYPE
        {
            let start = rd_u32(rec, offset_of!(MbrEntryLayout, starting_lba));
            let size = rd_u32(rec, offset_of!(MbrEntryLayout, size_in_lba));
            let canonical = rec[offset_of!(MbrEntryLayout, boot_indicator)] == 0x00
                && rec[offset_of!(MbrEntryLayout, start_chs)..offset_of!(MbrEntryLayout, os_type)] == [0x00, 0x02, 0x00]
                && rec[offset_of!(MbrEntryLayout, end_chs)..offset_of!(MbrEntryLayout, starting_lba)] == [0xFF, 0xFF, 0xFF];
            // 比较基准是**逻辑块**口径的 total，故 512 口径值
            // （PmbrSize::NeedsRepair{Compat512}）在此落空，会走到下面重写为规范值
            if canonical && start == 1 && size == want_size {
                return None;
            }
        }
    }
    // 保留引导代码，重写 446..510、块内保留区与签名
    protective_mbr_image(lba0, total_sectors);
    Some(lba0.to_vec())
}

/// 保护 MBR（LBA0）：内容由 [`protective_mbr_content`] 算定；已合规即一字不写。
/// 首次写盘前重读三扇区摘要并与入口记录的值比对（见 [`verify_table_stamp`]）。
/// 仅测试构建可见。
#[cfg(test)]
pub fn ensure_protective_mbr(src: &mut WriteSource, ss: u64) -> Result<(), RunOutcome> {
    let stamp = table_stamp(src, ss)?;
    let Some(lba0) = protective_mbr_content(src, ss)? else {
        return Ok(());
    };
    verify_table_stamp(src, stamp, ss)?;
    src.write_fixture(0, &lba0).map_err(RunOutcome::from)?;
    src.sync_all().map_err(RunOutcome::from)?;
    // 读回复核：写调用与 sync 只说明字节交给了设备，缓存丢写或镜像层短写要在重读
    // 时才可见。LBA0 被丢掉而这里报成功，盘随后会被判成 GPT damaged，写命令全拒
    let mut got = vec![0u8; lba0.len()];
    src.read_at(0, &mut got).map_err(|e| {
        RunOutcome::from(io::Error::new(e.kind(), format!("protective MBR read-back failed: {e} — the write may not have landed")))
    })?;
    if got != lba0 {
        return Err(RunOutcome::from(io::Error::new(
            io::ErrorKind::InvalidData,
            "protective MBR differs from what was written — the device or image layer dropped the write",
        )));
    }
    Ok(())
}
