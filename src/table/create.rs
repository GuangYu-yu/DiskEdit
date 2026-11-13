//! 新表创建：GPT/MBR 的形状预检与落盘，以及类型 GUID 常量。

use super::*;

// ---------- 分区创建 / 删除（CLI: new / add / delete） ----------

/// GUID 熵源：时间 + 路径哈希（无第三方 rand 依赖；不承诺 UUIDv4 质量）。
/// disk GUID 与分区 unique GUID 共用播种
pub(crate) fn derive_guid(path: &std::path::Path) -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut h1 = std::collections::hash_map::RandomState::new().build_hasher();
    h1.write(path.to_string_lossy().as_bytes());
    h1.write_u128(t);
    let a = h1.finish();
    let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
    h2.write_u128(t);
    h2.write(path.to_string_lossy().as_bytes());
    let b = h2.finish();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
    // 置 RFC 4122 variant + version 4 位（尽力而为，非强保证）
    out[7] = (out[7] & 0x0F) | 0x40;
    out[8] = (out[8] & 0x3F) | 0x80;
    out
}

/// 磁盘上的分区表格式（`new --table` 的值域）。用枚举而不是字符串：非法取值在参数解析
/// 阶段就被拒，命令层拿到的一定是合法值，不必再校验一次
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableKind {
    Gpt,
    Mbr,
}

impl TableKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "gpt" => Some(Self::Gpt),
            "mbr" => Some(Self::Mbr),
            _ => None,
        }
    }

    /// 成功信息里回显的名字，与命令行取值一致
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpt => "gpt",
            Self::Mbr => "mbr",
        }
    }
}

/// 建表最小容量（扇区数）：2·span+4（last_lba ≥ 2·span+3）——first/last_usable
/// 可分配的紧约束，同时保证主数组 [2, 2+span) 与备数组 [last-span, last) 不重叠。
/// 512B → 68 扇区，4Kn → 12 扇区（GNU parted 对 512B 给出同一 68 下限）
pub(crate) fn gpt_min_sectors(ss: u64) -> io::Result<u64> {
    // 新建表的条目数与单条目大小取建表策略（[`DEFAULT_ENTRY_COUNT`] /
    // [`DEFAULT_ENTRY_SIZE`]），与盘上任何既有几何无关
    let geom = EntryArrayGeometry::new(ss, DEFAULT_ENTRY_SIZE, DEFAULT_ENTRY_COUNT, MAX_ARRAY_BYTES)?;
    Ok(2 * geom.lba_span() + 4)
}

/// "盘装不下 GPT"的唯一措辞：preflight（命令层写盘前拒绝，RunOutcome::refused）与
/// create_gpt（直调入口的兜底，io::Error）共用同一判据 gpt_min_sectors 与同一句话
pub(crate) fn gpt_too_small(min_sectors: u64, ss: u64) -> String {
    format!("target too small for a GPT (needs ≥{min_sectors} sectors at {ss}-byte sector size)")
}

/// `new` 写盘前的全部拒绝判据，命令层必须在任何写入之前先过它。
/// 拒绝返回 RunOutcome::refused（盘未被改动）。
pub fn create_gpt_preflight(size_bytes: u64, ss: u64) -> Result<(), RunOutcome> {
    let min_sectors = gpt_min_sectors(ss).map_err(|e| RunOutcome::refused(e.to_string()))?;
    if size_bytes / ss < min_sectors {
        return Err(RunOutcome::refused(gpt_too_small(min_sectors, ss)));
    }
    Ok(())
}

/// `new --table mbr` 写盘前的判据：MBR 写 LBA0、LBA1 与末 LBA 三段，可用区
/// `[1, last-1]` 至少要留 1 个 LBA。拒绝返回 RunOutcome::refused。
pub fn create_mbr_preflight(size_bytes: u64, ss: u64) -> Result<(), RunOutcome> {
    if size_bytes / ss < 3 {
        return Err(RunOutcome::refused(format!(
            "target too small for an MBR (needs ≥3 sectors at {ss}-byte sector size)"
        )));
    }
    Ok(())
}

/// 旧表里每个条目的字节区间 `(名字, 起点, 终点)`。GPT 走不进保护 MBR 形状前置的解析，
/// 旧的保护 MBR 被改坏时也能读到它的分区；再叠原样解析的 MBR 条目（LBA0 的 MBR 记录与
/// 盘中的 GPT 表互不排斥，两者都要算）。表读不出来时上抛——"看不见旧表"不能当成
/// "旧表没有分区"
fn existing_partition_ranges(src: &FileSource, ss: u64) -> io::Result<Vec<(String, u64, u64)>> {
    let mut out = Vec::new();
    match load_gpt_ignoring_pmbr_shape(src) {
        Ok(Some(g)) => {
            for (i, e) in g.entries.iter().enumerate() {
                if e.ending_lba == 0 {
                    continue;
                }
                out.push((
                    format!("GPT partition {}", i + 1),
                    e.starting_lba.saturating_mul(g.ss),
                    e.ending_lba.saturating_add(1).saturating_mul(g.ss),
                ));
            }
        }
        Ok(None) => {}
        Err(e) => return Err(into_io_error(e)),
    }
    if let Some(raw) = super::mbr::parse_mbr_raw(src)? {
        for p in &raw.parts {
            out.push((
                format!("MBR partition {}", p.num),
                u64::from(p.start_lba).saturating_mul(ss),
                (u64::from(p.start_lba) + u64::from(p.size_lba)).saturating_mul(ss),
            ));
        }
    }
    Ok(out)
}

/// `new --table gpt` 的表结构写入区间（主头 LBA1、主条目数组、盘尾备份区）与旧表分区的冲突判定。
/// 主数组按新表几何算；旧表读不出来时报错而非静默返回 None。
/// 返回可读的冲突描述，命令层据此在写盘前拒绝。
pub fn gpt_metadata_conflict(src: &FileSource, ss: u64) -> io::Result<Option<String>> {
    let geom = EntryArrayGeometry::new(ss, DEFAULT_ENTRY_SIZE, DEFAULT_ENTRY_COUNT, MAX_ARRAY_BYTES)?;
    let span = geom.lba_span();
    let last_lba = container_last_lba(src, ss);
    let Some(first_lba) = last_lba.checked_sub(span) else {
        return Ok(None);
    };
    // 写入区间按字节表达：(名字, 起点, 终点)。主头在 LBA1，其后是主数组；备份数组起到
    // 容器尾，备份头在其后一并被这段覆盖
    let regions: [(&str, u64, u64); 3] = [
        ("primary GPT header", ss, 2 * ss),
        ("primary entry array", 2 * ss, (2 + span) * ss),
        ("backup entry array", first_lba.saturating_mul(ss), src.size),
    ];
    for (name, start, end) in existing_partition_ranges(src, ss)? {
        if let Some((what, region_start, region_end)) =
            regions.iter().find(|(_, rs, re)| end > *rs && start < *re)
        {
            return Ok(Some(format!(
                "{name} occupies bytes [{start}..{end}), overlapping the {what} this new table \
                 writes at [{region_start}..{region_end}) — delete or shrink that partition first"
            )));
        }
    }
    Ok(None)
}

/// `new --table mbr` 会清零的 GPT 签名位置（字节偏移）。判据与写入路径同源：候选扇区大小
/// 各自的 LBA1 与盘尾备份头；容器装不下该候选时该位置不存在。只在签名匹配的位置返回
/// ——`new --table mbr` 也只在这些位置写零
pub(crate) fn gpt_signature_sites(src: &FileSource) -> io::Result<Vec<u64>> {
    let ss = src.sector_size;
    let mut sites = Vec::new();
    for at in candidate_sector_sizes(ss) {
        if src.size < at.saturating_add(GPT_SIGNATURE.len() as u64) {
            continue;
        }
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        src.read_at(at, &mut sig)?;
        if &sig == GPT_SIGNATURE {
            sites.push(at);
        }
    }
    // 盘尾备份头按同一套候选：备份头的落点随表自身的扇区大小而变
    for at in candidate_sector_sizes(ss) {
        let off = container_last_lba(src, at).saturating_mul(at);
        // 不得落进容器的 LBA0：容器比该候选更小时盘尾会饱和到 LBA0 内部
        if off < ss || src.size < off.saturating_add(GPT_SIGNATURE.len() as u64) {
            continue;
        }
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        src.read_at(off, &mut sig)?;
        if &sig == GPT_SIGNATURE {
            sites.push(off);
        }
    }
    Ok(sites)
}

/// `new --table msdos` 的写盘前冲突判据：实际清零写集是 LBA1 与末 LBA 的整扇区，
/// 加上 [`gpt_signature_sites`] 里其余候选位置的 GPT 头签名。这些字节若落在旧表的分区里，
/// 清零就是改分区数据，需要覆盖授权。
///
/// 判定按**实际写集**，不按签名位置：整扇区清零与签名是否匹配无关，
/// 只看 8 字节签名位会漏掉同扇区里的分区数据
pub fn mbr_metadata_conflict(src: &FileSource, ss: u64) -> io::Result<Option<String>> {
    let parts = existing_partition_ranges(src, ss)?;
    let last_off = container_last_lba(src, ss).saturating_mul(ss);
    let mut cleared: Vec<(u64, u64, String)> = Vec::new();
    for (off, label) in [(ss, "LBA1"), (last_off, "the last LBA")] {
        let len = src.size.saturating_sub(off).min(ss);
        if len > 0 {
            cleared.push((off, off.saturating_add(len), format!("the whole {label} sector this msdos table zeros")));
        }
    }
    for off in gpt_signature_sites(src)? {
        // 整扇区已经覆盖的位置不重复登记
        if off == ss || off == last_off {
            continue;
        }
        cleared.push((off, off.saturating_add(GPT_SIGNATURE.len() as u64), "the GPT header bytes this msdos table clears".to_string()));
    }
    for (off, end, what) in cleared {
        for (name, start, p_end) in &parts {
            if end > *start && off < *p_end {
                return Ok(Some(format!(
                    "{name} occupies bytes [{start}..{p_end}), overlapping {what} at [{off}..{end})"
                )));
            }
        }
    }
    Ok(None)
}

/// `new` 两族表的统一冲突判据：既有分区是否落在本次写表会改动的字节上。
/// 族的分派只有这一处，命令层与写入口不各自挑族。
pub(crate) fn new_table_conflict(src: &FileSource, ss: u64, kind: TableKind) -> io::Result<Option<String>> {
    match kind {
        TableKind::Gpt => gpt_metadata_conflict(src, ss),
        TableKind::Mbr => mbr_metadata_conflict(src, ss),
    }
}

/// `create_gpt`/`create_mbr` 写入口的冲突闸，仅在测试构建中可见：与命令层的
/// [`new_table_conflict`] 同一判据，未授权覆盖分区数据时，冲突与"旧表读不出来"都按拒绝
/// 返回（确定未写盘）；授权时放行——`--overwrite-data` 的含义就是接受覆盖那些字节，
/// 旧表读不出来也不改变本次写入会落到的位置。
#[cfg(test)]
pub(crate) fn ensure_new_table_write_clear(
    src: &FileSource,
    ss: u64,
    kind: TableKind,
    overwrite_partition_data: bool,
) -> Result<(), RunOutcome> {
    match (new_table_conflict(src, ss, kind), overwrite_partition_data) {
        (Ok(None), _) => Ok(()),
        (Ok(Some(_)), true) | (Err(_), true) => Ok(()),
        (Ok(Some(why)), false) => Err(RunOutcome::refused(format!(
            "{why} — pass --overwrite-partition-data to accept overwriting those bytes"
        ))),
        (Err(e), false) => Err(RunOutcome::refused(format!(
            "cannot check the existing partition table against the bytes this new table writes ({e}) — \
             pass --overwrite-partition-data to proceed without that check"
        ))),
    }
}

/// 盘上是否已有可识别的表（GPT 或 msdos）。与 [`existing_partition_ranges`] 走同一套
/// 探测：GPT 不受保护 MBR 形状约束，msdos 认 LBA0 的签名与记录。表读不出来时上抛，
/// 调用方不得把"探测不出事实"当成"盘上没有表"放行。
pub(crate) fn recognized_table(src: &FileSource) -> io::Result<bool> {
    match load_gpt_ignoring_pmbr_shape(src) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(super::mbr::parse_mbr_raw(src)?.is_some()),
        Err(e) => Err(into_io_error(e)),
    }
}

/// 表探测的三态。空盘与"有数据但读不出表"必须分开：前者无需覆盖授权，
/// 后者不能断言写入只落在表结构区，也不能把它的非零字节当成空盘
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableProbe {
    /// 三处表扇区全零，且解析不出表
    Empty,
    /// 有非零数据，但解析不出可识别的表
    Unrecognized,
    /// 盘上有一份可解析的 GPT 或 msdos 表
    Recognized,
}

impl TableProbe {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Unrecognized => "unrecognized-non-empty",
            Self::Recognized => "recognized",
        }
    }

    /// 由同一次快照得到的两个判定合成三态：识别结果与三处表扇区是否全零。
    /// 两者都不成立才是"非空但读不出表"，不把这一态折进空盘
    pub(crate) fn classify(recognized: bool, blank: bool) -> Self {
        if recognized {
            Self::Recognized
        } else if blank {
            Self::Empty
        } else {
            Self::Unrecognized
        }
    }
}

/// `new --table gpt` 的计划期内容：空条目表 + 新盘 GUID + 按容器算出的表几何。
/// 纯计算、不读盘；写入与比对由命令层装配后执行。
pub(crate) fn new_gpt_content(src: &FileSource, disk_guid: Option<[u8; 16]>) -> Result<GptContent, RunOutcome> {
    // 与 create_gpt_preflight 同一判据：命令层已先过一遍，这里保证几何推导不下溢
    let min_sectors = gpt_min_sectors(src.sector_size).map_err(|e| RunOutcome::refused(e.to_string()))?;
    let ss = src.sector_size;
    if src.size / ss < min_sectors {
        return Err(RunOutcome::refused(gpt_too_small(min_sectors, ss)));
    }
    let geom = EntryArrayGeometry::new(ss, DEFAULT_ENTRY_SIZE, DEFAULT_ENTRY_COUNT, MAX_ARRAY_BYTES)
        .map_err(|e| RunOutcome::refused(e.to_string()))?;
    let span = geom.lba_span();
    let last_lba = container_last_lba(src, ss);
    let header = RawHeader {
        revision: 0x0001_0000,
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: 2 + span,
        last_usable_lba: last_lba - span - 1,
        disk_guid: disk_guid.unwrap_or_else(|| derive_guid(&src.path)),
        partition_entry_lba: 2,
        number_of_partition_entries: geom.entry_count,
        size_of_partition_entry: geom.entry_size,
        header_size: 92,
    };
    Ok(GptContent { ss, header, entries: vec![empty_entry(); geom.entry_count as usize], last_lba })
}

/// `new`：新建空 GPT（覆盖现有表）+ 保护 MBR。写盘只落在表结构区：LBA0/1、主条目数组
/// 与盘尾备份数组；旧表把分区铺进这些区域的提交由 [`ensure_new_table_write_clear`]
/// 在首次写盘前判掉——命令层的那次判定与这里之间隔着占用闸的 loop 扫描。
/// 几何：128 条目 × 128B，first_usable = 数组之后，last_usable = 末端 - span - 1
/// （由 UEFI 头/数组布局推导的实现约定，非规范逐字给出的公式）。
/// 仅在测试构建中可见：生产路径由命令层装配后执行。
#[cfg(test)]
pub fn create_gpt(
    src: &mut WriteSource,
    ss: u64,
    disk_guid: Option<[u8; 16]>,
    overwrite_partition_data: bool,
) -> Result<(), RunOutcome> {
    // 写前复核的基准：占用判定与几何推导都在这三扇区读定之后进行
    let stamp = table_stamp(src, ss)?;
    ensure_new_table_write_clear(src, ss, TableKind::Gpt, overwrite_partition_data)?;
    // 与 create_gpt_preflight 同一判据（同一 gpt_min_sectors）、同一措辞：命令层已先在
    // 写盘前过了一遍，这里是直接调本函数的调用方（测试）的兜底，两种入口看到同一句话
    let min_sectors = gpt_min_sectors(ss).map_err(|e| RunOutcome::refused(e.to_string()))?;
    if src.size / ss < min_sectors {
        return Err(RunOutcome::refused(gpt_too_small(min_sectors, ss)));
    }
    let geom = EntryArrayGeometry::new(ss, DEFAULT_ENTRY_SIZE, DEFAULT_ENTRY_COUNT, MAX_ARRAY_BYTES)?;
    let span = geom.lba_span();
    let last_lba = container_last_lba(src, ss);
    let header = RawHeader {
        revision: 0x0001_0000,
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: 2 + span,
        last_usable_lba: last_lba - span - 1,
        disk_guid: disk_guid.unwrap_or_else(|| derive_guid(&src.path)),
        partition_entry_lba: 2,
        number_of_partition_entries: geom.entry_count,
        size_of_partition_entry: geom.entry_size,
        header_size: 92,
    };
    let g = RawGpt { ss, header, entries: vec![empty_entry(); geom.entry_count as usize], state: GptState::Valid, pmbr: PmbrSize::Normal };
    verify_table_stamp(src, stamp, ss)?;
    commit_gpt(src, &g, last_lba)?;
    ensure_protective_mbr(src, ss)?;
    Ok(())
}

pub(crate) fn empty_entry() -> GPTPartitionEntry {
    GPTPartitionEntry {
        partition_type_guid: [0; 16],
        unique_partition_guid: [0; 16],
        starting_lba: 0,
        ending_lba: 0,
        attribute_bits: 0,
        partition_name: "".into(),
    }
}

// 表项编排（add/rename/flag/delete：读 → 判 → 修复 → 提交）在策略层（gpt_policy）；本层只保留写入原语与常量

/// GPT flags：attribute 位按 UEFI 2.10 §5（bit0=Required Partition，bit1=No Block IO
/// Protocol 即 hidden，bit2=Legacy BIOS Bootable）；esp/boot 为类型 GUID 切换
/// EFI System Partition 类型 GUID（内核 block/partitions/efi.h PARTITION_SYSTEM_GUID）。
/// 磁盘字节序：UEFI GUID 前 3 字段小端落盘（内核 include/linux/efi.h EFI_GUID 宏按
/// a&0xff,(a>>8)&0xff,… 展开），对应标准文本 C12A7328-F81F-11D2-BA4B-00A0C93EC93B
pub const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// Linux filesystem data 类型 GUID（util-linux libfdisk/src/gpt.c GPT_DEFAULT_ENTRY_TYPE，
/// 标准文本 0FC63DAF-8483-4772-8E79-3D69D8477DE4 的磁盘字节序）
pub const LINUX_FS_TYPE_GUID: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// Microsoft basic data 类型 GUID（标准文本 A2A0D0EB-E5B9-3344-87C0-68B6B72699C7 的磁盘字节序）。
/// `flag esp=off` 对 FAT/NTFS 分区的回退落点（parted gpt.c set_system：fs_type 为
/// fat/ntfs 时落此类型而非 Linux data）
pub const BASIC_DATA_TYPE_GUID: [u8; 16] = [
    0xEB, 0xD0, 0xA0, 0xA2, 0xB9, 0xE5, 0x44, 0x33, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

/// Linux swap 类型 GUID（标准文本 0657FD6D-A4AB-43C4-84E5-0933C84B4F4F 的磁盘字节序）。
/// movepart 依赖此识别 swap 挡路者（不搬数据、落位后 mkswap 重建）
pub const SWAP_TYPE_GUID: [u8; 16] = [
    0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
];

/// Linux LUKS 类型 GUID（util-linux libfdisk/src/pt-gpt-partnames.h "Linux LUKS"，
/// 标准文本 CA7D7CCB-63ED-4C53-861C-1742536059CC 的磁盘字节序）。
/// movepart 依赖此拒绝搬移加密容器：位移后其元数据语义不明
pub const LUKS_TYPE_GUID: [u8; 16] = [
    0xCB, 0x7C, 0x7D, 0xCA, 0xED, 0x63, 0x53, 0x4C, 0x86, 0x1C, 0x17, 0x42, 0x53, 0x60, 0x59, 0xCC,
];

/// Linux LVM 类型 GUID（util-linux libfdisk/src/pt-gpt-partnames.h "Linux LVM"，
/// 标准文本 E6D6D379-F507-44C2-A23C-238F2A3DF928 的磁盘字节序）。
/// movepart 依赖此拒绝搬移 PV
pub const LVM_TYPE_GUID: [u8; 16] = [
    0x79, 0xD3, 0xD6, 0xE6, 0x07, 0xF5, 0xC2, 0x44, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28,
];

/// 测试夹具：把规范 EFI 编码的 GUID 转成前三个字段倒序的摆放，用来构造必须被拒绝的样本。
/// 识别侧只比较规范编码，不认这种摆放；写路径的常量也一律是规范编码
/// （[`ESP_TYPE_GUID`] 等），不经过本函数
#[cfg(test)]
pub(crate) fn guid_in_field_order(g: &[u8; 16]) -> [u8; 16] {
    let mut out = *g;
    out[0..4].reverse();
    out[4..6].reverse();
    out[6..8].reverse();
    out
}

// ---------- msdos（真 MBR）表创建与主分区条目编辑 ----------
// 限制：仅主分区槽位 1..=4，不支持扩展/逻辑分区链。

/// `new --table msdos`：写空 MBR（保留 BootCode，置签名，清空 4 条记录），并清掉
/// LBA1 与盘尾备份头上可能残留的 GPT 签名——新表是 msdos，盘上不该再留下任何能独立
/// 给出 GPT 表的表层。只清签名那 8 字节，同一扇区的其余字节与条目数原样保留。
/// 主头签名留着会让这块新 msdos 盘被判成 `GptDamaged`（`table_label` 的第 3 条判据）；
/// 盘尾那个留着会让"4 条记录全空"的判据把刚建的盘判成 GPT 残骸。
/// 会清零的签名若落在旧分区数据里，由 [`ensure_new_table_write_clear`] 在首次写盘前判掉。
/// 仅在测试构建中可见：生产路径由命令层装配后执行。
#[cfg(test)]
pub fn create_mbr(src: &mut WriteSource, overwrite_partition_data: bool) -> Result<(), RunOutcome> {
    let stamp = table_stamp(src, src.sector_size)?;
    ensure_new_table_write_clear(src, src.sector_size, TableKind::Mbr, overwrite_partition_data)?;
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0)?;
    lba0[446..510].fill(0);
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    // 与解析侧同一套候选扇区大小：容器 ss 之外的候选说明旧 GPT 可能按另一种扇区
    // 落盘，签名会落在那个候选的 LBA1 上。清除位置与写回缓冲是同一个事实的两半：
    // 落在 LBA0/LBA1 缓冲内的位置直接施加到缓冲，其余位置先写出去，最后整块回写。
    // 先写 LBA0 再清 ss 会在 `at == ss` 的位置造出"新 msdos 表 + 可解析的 GPT 主头"
    // ——`GptDamaged`，工具内没有修复入口。
    // 待清位置与冲突检查共用 `gpt_signature_sites`，两者不会各自漂移
    let lba1_whole = src.size >= 2 * ss;
    // LBA0 与 LBA1 上"不再有 GPT 主头"是同一个事实的两半：分两次写出去，中断正好停在
    // GptDamaged 上。拼进一个缓冲、一次 write，中断只可能停在"旧表"或"新表"
    let mut head = lba0.clone();
    if lba1_whole {
        let mut lba1 = vec![0u8; ss as usize];
        src.read_at(ss, &mut lba1)?;
        if lba1[..GPT_SIGNATURE.len()] == *GPT_SIGNATURE {
            lba1[..GPT_SIGNATURE.len()].fill(0);
            head.extend_from_slice(&lba1);
        }
    }
    let mut cleared = Vec::new();
    verify_table_stamp(src, stamp, src.sector_size)?;
    for off in gpt_signature_sites(src)? {
        cleared.push(off);
        if off.saturating_add(GPT_SIGNATURE.len() as u64) <= head.len() as u64 {
            let at = off as usize;
            head[at..at + GPT_SIGNATURE.len()].fill(0);
        } else {
            src.write_fixture(off, &[0u8; GPT_SIGNATURE.len()])?;
        }
    }
    src.write_fixture(0, &head)?;
    src.sync_all()?;
    // 读回复核：比的是**意图**（新 LBA0，以及被清掉签名的那些位置），不是刚写下去的
    // 缓冲——写被缓存/镜像层丢掉时，盘会在返回 Ok 之后被判成 GptDamaged
    let mut got = vec![0u8; lba0.len()];
    src.read_at(0, &mut got).map_err(|e| {
        io::Error::new(e.kind(), format!("MBR read-back failed: {e} — the write may not have landed"))
    })?;
    if got != head[..lba0.len()] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MBR differs from what was written — the device or image layer dropped the write",
        )
        .into());
    }
    for at in cleared {
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        src.read_at(at, &mut sig)?;
        if sig != [0u8; GPT_SIGNATURE.len()] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("GPT signature at byte {at} was not cleared — the device or image layer dropped the write"),
            )
            .into());
        }
    }
    Ok(())
}

/// 盘型。**与"表的形状是否完好"分开表达**：`GptDamaged` 是"LBA1 带 GPT 头签名、
/// 但保护布局不满足"——盘型仍是 GPT，只是形状受损，故与 `Gpt` 分开（处置不同）。
/// 展示层各自格式化，内部判定只认变体
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableLabel {
    Gpt,
    GptDamaged,
    Mbr,
    None,
}

impl TableLabel {
    /// 形状受损。**GPT 侧专用**：MBR 的损伤由 `RawMbr::damage` 表达——两个表族的判据
    /// 不同源，只在 `info` 的 `damaged` 字段处汇合
    pub fn is_damaged(self) -> bool {
        self == TableLabel::GptDamaged
    }
}

/// 人读文案。**只用于给人看的措辞**，不是机器接口的一部分：脚本读的是 `info` 的结构化
/// 字段（`label` + `damaged`），两者各自独立、互不推导，改这里的措辞不会影响那个契约
impl std::fmt::Display for TableLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TableLabel::Gpt => "gpt",
            TableLabel::GptDamaged => "gpt (damaged)",
            TableLabel::Mbr => "msdos",
            TableLabel::None => "none",
        })
    }
}

/// 盘型判定。
/// MBR 解析的 io 失败也走 GptError::Io（本函数只回答盘型，不区分来源）
pub fn table_label(src: &FileSource) -> Result<TableLabel, GptError> {
    if load_gpt(src)?.is_some() {
        return Ok(TableLabel::Gpt);
    }
    // 保护 MBR 形状成立而两份 GPT 头都给不出表：这是一块 GPT 盘，只是头毁到
    // 签名尽失（条目数组可能完好，仍可救）。落成 None 会让 new 把它当无表盘
    // 覆盖、resize 走 superfloppy 整盘扩——与下方签名路径同一处置，交给写命令拒绝
    if pmbr_shape_valid(src)? {
        return Ok(TableLabel::GptDamaged);
    }
    // LBA1 仍有可解析的主头（签名 + HeaderCRC32）：这是一块 GPT 盘，只是 LBA0 的保护
    // 布局被改坏。若在此放行 parse_mbr，一个被改成真实类型的保护记录会让整盘判成
    // msdos，后续写入只改 MBR 视图、GPT 视图不动，两套表静默分叉
    if gpt_header_parseable(src)? {
        return Ok(TableLabel::GptDamaged);
    }
    // 用原样解析而非 `parse_mbr`：后者对条目越界/起始 LBA0/两两重叠一律 Err，盘型判定
    // 不该因为表损坏而失败——损坏的 MBR 仍是 MBR，`info` 的 `label` 与 `damaged` 是
    // 两个独立事实。写路径要的是"可操作的表"，它们各自仍经 `parse_mbr` 拒绝
    if let Some(raw) = parse_mbr_raw(src)? {
        // 4 条记录全空时，LBA0 的形状与"主头被毁、只剩盘尾备份的 GPT 盘"相同。备份若
        // 自洽到能给出整张表，那块盘的盘型是 GPT——报 msdos 会让 add 与 msdos resize 把
        // 分区铺到盘尾，覆盖唯一可救的那份备份。本工具建的 msdos 盘不会落进这一支：
        // `create_mbr` 连盘尾备份头的签名一并清掉（见该函数）
        if raw.parts.is_empty() && super::gpt_parse::backup_table_parseable(src)? {
            return Ok(TableLabel::GptDamaged);
        }
        return Ok(TableLabel::Mbr);
    }
    // 这里探到 LBA1 签名而 load_gpt 已放行失败：保护布局不满足的 GPT 盘（保护 MBR
    // 被改写 / LBA0 被抹）。报 `GptDamaged` 而非 `None`——写命令对非 gpt/msdos
    // 标签一律拒绝，且用户看到的是"盘型可辨、布局受损"，不会被诱导用 `new` 覆盖
    // 一份或许还能救回的表；resize 也不会把这种盘当 superfloppy 整盘扩
    if gpt_signature_present(src)? {
        return Ok(TableLabel::GptDamaged);
    }
    // LBA0 连 MBR 签名都没有时不存在与之竞争的表：盘尾备份若自洽到能给出整张表，那是
    // 这块盘仅存的一份。落成 None 会让 new 当空盘覆盖掉它，故报 GptDamaged
    // （写命令对非 gpt/msdos 一律拒绝）
    if !super::mbr::mbr_signature_present(src)? && super::gpt_parse::backup_table_parseable(src)? {
        return Ok(TableLabel::GptDamaged);
    }
    Ok(TableLabel::None)
}
