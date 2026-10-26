//! GPT 的崩溃安全写入：序列化、双头规范化与提交。

use super::*;

pub(crate) fn encode_name(entry: &GPTPartitionEntry) -> [u8; 72] {
    let mut out = [0u8; 72];
    let s = entry.partition_name.as_str();
    let units: Vec<u16> = s.encode_utf16().take(36).collect();
    // 第 36 码元为高代理时其低代理落在截断界外，回退 35 个码元保住完整代理对
    let n = if units.len() == 36 && (0xD800..=0xDBFF).contains(&units[35]) { 35 } else { units.len() };
    for (i, u) in units[..n].iter().enumerate() {
        out[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    out
}

/// 条目序列化：128 字节规范布局（UEFI 2.10 §5.3.3 GPT Partition Entry Array，Table 5.6）
pub(crate) fn serialize_entry(e: &GPTPartitionEntry) -> [u8; 128] {
    let mut b = [0u8; 128];
    b[0..16].copy_from_slice(&e.partition_type_guid);
    b[16..32].copy_from_slice(&e.unique_partition_guid);
    b[32..40].copy_from_slice(&e.starting_lba.to_le_bytes());
    b[40..48].copy_from_slice(&e.ending_lba.to_le_bytes());
    b[48..56].copy_from_slice(&e.attribute_bits.to_le_bytes());
    b[56..128].copy_from_slice(&encode_name(e));
    b
}

/// 头序列化：HeaderSize 字节有效 + 补零到扇区；CRC 对 HeaderSize 字节
/// （CRC 字段置 0）计算（UEFI 2.10 §5.3.2 Table 5.5：HeaderCRC32 覆盖 HeaderSize 字节，
/// HeaderSize ≥ 92 且 ≤ 逻辑块大小）。HeaderSize 之后到扇区末尾为保留区，恒写零
pub(crate) fn serialize_header(h: &RawHeader, array_crc: u32, ss: u64) -> io::Result<Vec<u8>> {
    if h.header_size < 92 || h.header_size as u64 > ss {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GPT HeaderSize {} out of range 92..={ss}", h.header_size),
        ));
    }
    let mut b = vec![0u8; ss as usize];
    b[0..8].copy_from_slice(GPT_SIGNATURE);
    b[8..12].copy_from_slice(&h.revision.to_le_bytes());
    b[12..16].copy_from_slice(&h.header_size.to_le_bytes());
    b[16..20].copy_from_slice(&0u32.to_le_bytes()); // CRC 占位
    b[24..32].copy_from_slice(&h.primary_lba.to_le_bytes());
    b[32..40].copy_from_slice(&h.backup_lba.to_le_bytes());
    b[40..48].copy_from_slice(&h.first_usable_lba.to_le_bytes());
    b[48..56].copy_from_slice(&h.last_usable_lba.to_le_bytes());
    b[56..72].copy_from_slice(&h.disk_guid);
    b[72..80].copy_from_slice(&h.partition_entry_lba.to_le_bytes());
    b[80..84].copy_from_slice(&h.number_of_partition_entries.to_le_bytes());
    b[84..88].copy_from_slice(&h.size_of_partition_entry.to_le_bytes());
    b[88..92].copy_from_slice(&array_crc.to_le_bytes());
    let crc = crc32(&b[..h.header_size as usize]);
    b[16..20].copy_from_slice(&crc.to_le_bytes());
    Ok(b)
}

/// 条目数组序列化（含补零到扇区边界），返回 (字节, 数组 CRC)。
/// 几何由 [`EntryArrayGeometry`] 单点判定（es > 128 时条目尾部保留区保持零；
/// 不合规的几何在构造点即报错，此处不再自证）
pub fn serialize_array(entries: &[GPTPartitionEntry], geom: &EntryArrayGeometry) -> (Vec<u8>, u32) {
    let span = geom.lba_span();
    let mut b = vec![0u8; (span * geom.sector_size) as usize];
    for (i, e) in entries.iter().enumerate().take(geom.entry_count as usize) {
        let off = i * geom.entry_size as usize;
        b[off..off + 128].copy_from_slice(&serialize_entry(e));
    }
    let crc = crc32(&b[..geom.byte_len() as usize]);
    (b, crc)
}

/// 重建规范化主/备头（写入路径：无论读到的是哪份副本，输出总为规范位置）
/// - 主头：MyLBA=1, Alt=last_lba, 数组=2
/// - 备头：MyLBA=last_lba, Alt=1, 数组=backup_array_lba（= last_lba − span）
///
/// `backup_array_lba` 由调用方算好传入（commit_gpt 已用 checked_sub 校验容器装得下数组）：
/// 本函数再算一次既与 serialize_array 的同一个跨度重复，又会先于调用方的下溢保护执行——
/// debug 下 panic、release 下先回绕再被调用方拦下，同一个事实两处推导
pub(crate) fn canonical_headers(h: &RawHeader, last_lba: u64, backup_array_lba: u64) -> (RawHeader, RawHeader) {
    let primary = RawHeader {
        revision: h.revision,
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: h.first_usable_lba,
        last_usable_lba: h.last_usable_lba,
        disk_guid: h.disk_guid,
        partition_entry_lba: 2,
        number_of_partition_entries: h.number_of_partition_entries,
        size_of_partition_entry: h.size_of_partition_entry,
        header_size: h.header_size,
    };
    let backup = RawHeader {
        primary_lba: last_lba,
        backup_lba: 1,
        first_usable_lba: h.first_usable_lba,
        last_usable_lba: h.last_usable_lba,
        disk_guid: h.disk_guid,
        partition_entry_lba: backup_array_lba,
        ..primary.clone()
    };
    (primary, backup)
}

/// 崩溃安全四结构序列：备数组 → 备头 → 主数组 → 主头，每步 sync。
/// 每个中间落点都留有一份 CRC 自洽的表；主备两份各自自洽而内容分叉时 CRC 检不出
/// （不一致只出现在跨副本比较），解析按主副本优先收敛——中断的代价是本次提交丢失，
/// 不是表损坏。
pub(crate) fn commit_gpt(src: &mut WriteSource, g: &RawGpt, last_lba: u64) -> io::Result<()> {
    commit_table(src, g.ss, &g.header, &g.entries, last_lba)
}

/// 崩溃安全四结构序列的唯一实现处：`commit_gpt`（解析侧产物）与
/// [`crate::geometry::ValidatedGeometry::commit`]（写入路径的可操作几何）都走这里，
/// 于是"提交一张表"只有一份序列、一份几何推导
pub(crate) fn commit_table(
    src: &mut WriteSource,
    ss: u64,
    header: &RawHeader,
    entries: &[GPTPartitionEntry],
    last_lba: u64,
) -> io::Result<()> {
    // 几何只构造一次：数组字节数、扇区跨度、条目数与大小都取自它，不再各自乘一遍
    let geom = EntryArrayGeometry::new(ss, header.size_of_partition_entry, header.number_of_partition_entries, MAX_ARRAY_BYTES)?;
    let (array_bytes, array_crc) = serialize_array(entries, &geom);
    // 跨度只算一次（几何对象），下溢检查先于任何头部构造
    let backup_array_lba = last_lba
        .checked_sub(geom.lba_span())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "disk too small to hold GPT entry array"))?;
    // 备数组必须排在主数组 [2, 2+span) 之后，且两者之间留出一个可用扇区（与
    // create.rs 的 gpt_min_sectors 同一判据）：只做上面的 checked_sub 时，last_lba 落在
    // [span, 2·span+2] 的提交会让备数组压到 LBA0/LBA1/主数组上，四段写互相覆盖，
    // 而写后读回比对的正是刚写进去的字节，损坏会以 Ok 返回
    let min_sectors = 2 * geom.lba_span() + 4;
    if last_lba.checked_add(1).is_none_or(|sectors| sectors < min_sectors) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, gpt_too_small(min_sectors, ss)));
    }
    let (primary, backup) = canonical_headers(header, last_lba, backup_array_lba);
    // 两份头的字节都在首次写盘之前构造完：构造会因 HeaderSize 越界而失败，
    // 那时盘必须还没被碰过（放到写作序列中间会让拒绝留下半张表）
    let bh = serialize_header(&backup, array_crc, ss)?;
    let ph = serialize_header(&primary, array_crc, ss)?;
    // 再用**将要写出的**两份头过一遍解析侧同一套几何与条目校验。字节级读回只能证明
    // "写进去什么"，证明不了"写进去的表是这个工具读得回来的表"：口径不一致的调用方
    // （另一种 ss 算的 last_lba、扩容前的头配新末端）会写出主备同时自相矛盾的表，
    // 读回全对、命令报成功，而随后的 load_gpt 判它 invalid——盘上再没有可用副本。
    // 校验器按主头视角定义，备份头先归一化再以 Backup 视角过同一组判据
    let file_last_lba = (src.size / ss).saturating_sub(1);
    if last_lba > file_last_lba {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GPT end LBA {last_lba} is beyond the container end {file_last_lba} — refusing to write the table"),
        ));
    }
    let backup_view = RawHeader { primary_lba: 1, backup_lba: last_lba, ..backup.clone() };
    for (h, view, what) in
        [(&primary, GptCopyKind::Primary, "primary"), (&backup_view, GptCopyKind::Backup, "backup")]
    {
        validate_geometry(h, &geom, last_lba, view).map_err(into_io_error).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the {what} GPT header this write would produce is not self-consistent: {e}"),
            )
        })?;
    }
    validate_entries(entries, &primary).map_err(into_io_error).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("the partition entries this write would produce are invalid: {e}"))
    })?;

    src.write_at(backup_array_lba * ss, &array_bytes)?;
    src.sync_all()?;

    src.write_at(last_lba * ss, &bh)?;
    src.sync_all()?;

    src.write_at(2 * ss, &array_bytes)?;
    src.sync_all()?;

    src.write_at(ss, &ph)?;
    src.sync_all()?;
    // 读回复核：`sync_all` 只是"设备声称已落盘"，缓存丢写与镜像层短写都不在其中。
    // 修复路径（`gpt_policy::apply_repair`）对同一承诺做重读并把不符判失败，
    // 常规提交同样不能只凭写调用的返回值就报成功。四条结构比对写入时的字节，
    // 覆盖头 CRC、数组 CRC 与条目数三件事（它们都体现在这份字节里）
    for (want, off, what) in [
        (&array_bytes, backup_array_lba * ss, "backup GPT entry array"),
        (&bh, last_lba * ss, "backup GPT header"),
        (&array_bytes, 2 * ss, "primary GPT entry array"),
        (&ph, ss, "primary GPT header"),
    ] {
        let mut got = vec![0u8; want.len()];
        src.read_at(off, &mut got).map_err(|e| {
            io::Error::new(e.kind(), format!("{what} read-back failed: {e} — the table may not have been written"))
        })?;
        if got != *want {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{what} differs from what was written — the device or image layer dropped the write"),
            ));
        }
    }
    Ok(())
}

/// 规范 SizeInLBA = 逻辑块数 − 1；逻辑块数超出 32 位表示范围时按规范饱和写 0xFFFFFFFF
/// （UEFI 2.10 §5.2.3，LBA 字段以 logical block 计而非恒定 512 字节）。
/// "写入什么值"（[`ensure_protective_mbr`]）与"读到的值算不算规范"
/// （[`pmbr_size_state`]）共用这一处，两侧不各写一遍同一个算式
pub(crate) fn size_in_lba(total_sectors: u64) -> u32 {
    if total_sectors > u32::MAX as u64 {
        u32::MAX
    } else {
        total_sectors.saturating_sub(1) as u32
    }
}

/// 保护 MBR（LBA0）：保留 BootCode 区 0..446，重写 446..510 与块内保留区。
/// SizeInLBA = 逻辑块数 − 1（UEFI 2.10 §5.2.3 Table 5.4；单位是 logical block，非恒定
/// 512 字节），超出 32 位表示范围才用 0xFFFFFFFF。本函数同时是规范化点：任何非规范值
/// （stale / 512 口径）经此重写即收敛。
///
/// `ss` 是**该表自身**的逻辑块大小（不是容器的 `src.sector_size`）：两者不等时
/// （4Kn 表放在 512 字节镜像里）按容器换算会整体差 8 倍。同一次写入同时覆盖
/// `512..ss` 的块内保留区并按规范清零（Table 5.3 "Reserved … Set to zero"）
pub fn ensure_protective_mbr(src: &mut WriteSource, ss: u64) -> io::Result<()> {
    // 零扇区盘的 read_at(0) 只会撞出 UnexpectedEof，把"盘没有扇区"这件事
    // 说成一次 I/O 意外：先于任何读盘把几何前提判掉
    let total_sectors = src.size / ss;
    if total_sectors == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "disk has zero sectors"));
    }
    let want_size = size_in_lba(total_sectors);
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0)?;
    let reserved_zero = lba0[512..].iter().all(|&b| b == 0);
    if lba0[510] == 0x55 && lba0[511] == 0xAA && reserved_zero {
        let rec = &lba0[446..462]; // 槽位 1（与下方写入位置一致）
        // 与 pmbr_shape_valid 同口径：槽位 2-4 必须全零，否则 hybrid MBR 残留
        // 会使 load_gpt 拒读 GPT（形状前置不满足），盘型判定就此失真
        if lba0[462..510].iter().all(|&b| b == 0) && rec[4] == PROT_MBR_TYPE {
            let start = rd_u32(rec, 8);
            let size = rd_u32(rec, 12);
            // 已合规即一字不写（保留外层引导代码）。比较基准是**逻辑块**口径的 total，
            // 故 512 口径值（PmbrSize::NeedsRepair{Compat512}）在此落空，会走到下面重写为规范值
            if start == 1 && size == want_size {
                return Ok(());
            }
        }
    }
    // 保留引导代码，重写 446..510、块内保留区与签名
    lba0[446..510].fill(0);
    lba0[512..].fill(0);
    let rec = &mut lba0[446..462];
    rec[0] = 0x00; // BootIndicator
    rec[1..4].copy_from_slice(&[0x00, 0x02, 0x00]); // StartCHS 惯例值
    rec[4] = PROT_MBR_TYPE;
    rec[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // EndCHS
    rec[8..12].copy_from_slice(&1u32.to_le_bytes());
    rec[12..16].copy_from_slice(&want_size.to_le_bytes());
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    src.write_at(0, &lba0)?;
    src.sync_all()?;
    // 读回复核：写调用与 sync 只说明字节交给了设备，缓存丢写或镜像层短写要在重读
    // 时才可见。LBA0 被丢掉而这里报成功，盘随后会被判成 GPT damaged，写命令全拒
    let mut got = vec![0u8; lba0.len()];
    src.read_at(0, &mut got).map_err(|e| {
        io::Error::new(e.kind(), format!("protective MBR read-back failed: {e} — the write may not have landed"))
    })?;
    if got != lba0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "protective MBR differs from what was written — the device or image layer dropped the write",
        ));
    }
    Ok(())
}
