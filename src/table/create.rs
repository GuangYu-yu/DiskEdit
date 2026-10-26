//! 新表创建：GPT/MBR 的形状预检与落盘，以及类型 GUID 常量。

use super::*;

// ---------- 分区创建 / 删除（CLI: new / add / del） ----------

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
    Msdos,
}

impl TableKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "gpt" => Some(Self::Gpt),
            "msdos" => Some(Self::Msdos),
            _ => None,
        }
    }

    /// 成功信息里回显的名字，与命令行取值一致
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpt => "gpt",
            Self::Msdos => "msdos",
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

/// "盘装不下 GPT"的唯一措辞：preflight（命令层写盘前拒绝，Fail::refused）与
/// create_gpt（直调入口的兜底，io::Error）共用同一判据 gpt_min_sectors 与同一句话
pub(crate) fn gpt_too_small(min_sectors: u64, ss: u64) -> String {
    format!("target too small for a GPT (needs ≥{min_sectors} sectors at {ss}-byte sector size)")
}

/// `new` 写盘前的全部拒绝判据。命令层必须在任何写入之前先过它：这些检查若埋在
/// create_gpt 里以 io::Error 上抛，会被"建表失败可能停在中间态"的映射误报成
/// Failed（30 + "盘可能已改变"）——而此刻什么都没写，语义是 refused（10）
pub fn create_gpt_preflight(size_bytes: u64, ss: u64) -> Result<(), Fail> {
    let min_sectors = gpt_min_sectors(ss).map_err(|e| Fail::refused(e.to_string()))?;
    if size_bytes / ss < min_sectors {
        return Err(Fail::refused(gpt_too_small(min_sectors, ss)));
    }
    Ok(())
}

/// `new --table msdos` 写盘前的判据：MBR 只占 LBA0，但盘至少要有一个扇区可写。
/// 与 GPT 侧同一理由：把"盘太小"当建表失败的 io::Error 报，会被误报成 Failed
pub fn create_mbr_preflight(size_bytes: u64, ss: u64) -> Result<(), Fail> {
    if size_bytes / ss < 1 {
        return Err(Fail::refused(format!(
            "target too small for an MBR (needs ≥1 sector at {ss}-byte sector size)"
        )));
    }
    Ok(())
}

/// `new --table gpt` 的两处表结构写入：主条目数组（`[2·ss, (2+span)·ss)`）与盘尾备份区
/// （`[last−span, last]` 及其后的备份头）。两处缓冲在空表时都来自同一份全零数组。旧表
/// 若有分区落在任一段，这次写入就是清零分区数据，与 `new` 只破坏表结构的语义相反。
/// 返回可读的冲突描述，命令层据此在写盘前拒绝。
///
/// 主数组同样按新表几何算：旧表的 `first_usable_lba` 可以小于 `2+span`（条目数远小于
/// 建表策略的 128 时，旧表允许从 LBA 4 起分区），只算备份段会让那段分区开头被空数组清零。
///
/// 旧表读不出来时返回 `None`：`new` 本就是整体覆盖表的命令，此处只拦**可证**的相交，
/// 不因一张读不出的表拒绝一条用于重建它的命令
pub fn gpt_metadata_conflict(src: &FileSource, ss: u64) -> io::Result<Option<String>> {
    let geom = EntryArrayGeometry::new(ss, DEFAULT_ENTRY_SIZE, DEFAULT_ENTRY_COUNT, MAX_ARRAY_BYTES)?;
    let span = geom.lba_span();
    let last_lba = container_last_lba(src, ss);
    let Some(first_lba) = last_lba.checked_sub(span) else {
        return Ok(None);
    };
    // 写入区间按字节表达：(名字, 起点, 终点)。备份数组之后还要写备份头，故到容器尾
    let regions: [(&str, u64, u64); 2] = [
        ("primary entry array", 2 * ss, (2 + span) * ss),
        ("backup entry array", first_lba.saturating_mul(ss), src.size),
    ];
    let conflict = |start: u64, end: u64| {
        regions
            .iter()
            .find(|(_, region_start, region_end)| end > *region_start && start < *region_end)
    };
    if let Ok(Some(g)) = load_gpt(src) {
        for (i, e) in g.entries.iter().enumerate() {
            if e.ending_lba == 0 {
                continue;
            }
            let start = e.starting_lba.saturating_mul(g.ss);
            let end = e.ending_lba.saturating_add(1).saturating_mul(g.ss);
            if let Some((what, region_start, region_end)) = conflict(start, end) {
                return Ok(Some(format!(
                    "GPT partition {} occupies bytes [{start}..{end}), overlapping the {what} this new table \
                     writes at [{region_start}..{region_end}) — delete or shrink that partition first",
                    i + 1
                )));
            }
        }
    }
    if let Some(raw) = super::mbr::parse_mbr_raw(src)? {
        for p in &raw.parts {
            let start = u64::from(p.start_lba).saturating_mul(ss);
            let end = (u64::from(p.start_lba) + u64::from(p.size_lba)).saturating_mul(ss);
            if let Some((what, region_start, region_end)) = conflict(start, end) {
                return Ok(Some(format!(
                    "MBR partition {} occupies bytes [{start}..{end}), overlapping the {what} this new table \
                     writes at [{region_start}..{region_end}) — delete or shrink that partition first",
                    p.num
                )));
            }
        }
    }
    Ok(None)
}

/// `new`：新建空 GPT（覆盖现有表）+ 保护 MBR。写盘只落在表结构区：LBA0/1、主条目数组
/// 与盘尾备份数组；旧表把分区铺进这些区域的提交由命令层先经
/// [`gpt_metadata_conflict`] 拒绝，不在这里自查。
/// 几何：128 条目 × 128B，first_usable = 数组之后，last_usable = 末端 - span - 1
/// （由 UEFI 头/数组布局推导的实现约定，非规范逐字给出的公式）。
pub fn create_gpt(src: &mut WriteSource, ss: u64, disk_guid: Option<[u8; 16]>) -> io::Result<()> {
    // 与 create_gpt_preflight 同一判据（同一 gpt_min_sectors）、同一措辞：命令层已先在
    // 写盘前过了一遍，这里是直接调本函数的调用方（测试）的兜底，两种入口看到同一句话
    let min_sectors = gpt_min_sectors(ss)?;
    if src.size / ss < min_sectors {
        return Err(io::Error::new(io::ErrorKind::InvalidData, gpt_too_small(min_sectors, ss)));
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
    commit_gpt(src, &g, last_lba)?;
    ensure_protective_mbr(src, ss)
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

// 表项编排（add/rename/flag/del：读 → 判 → 修复 → 提交）已上移策略层
// （gpt_policy）；本层只保留它们用到的写入原语与常量

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

// ---------- msdos（真 MBR）表创建与主分区条目编辑 ----------
// 限制：仅主分区槽位 1..=4，不支持扩展/逻辑分区链。

/// `new --table msdos`：写空 MBR（保留 BootCode，置签名，清空 4 条记录），并清掉
/// LBA1 与盘尾备份头上可能残留的 GPT 签名——新表是 msdos，盘上不该再留下任何能独立
/// 给出 GPT 表的表层。只清签名那 8 字节，同一扇区的其余字节与条目数原样保留。
/// 主头签名留着会让这块新 msdos 盘被判成 `GptDamaged`（`table_label` 的第 3 条判据）；
/// 盘尾那个留着会让"4 条记录全空"的判据（第 5 条）把刚建的盘判成 GPT 残骸
pub fn create_mbr(src: &mut WriteSource) -> io::Result<()> {
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0)?;
    lba0[446..510].fill(0);
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    // 与解析侧同一套候选扇区大小：容器 ss 之外的候选说明旧 GPT 可能按另一种扇区
    // 落盘，签名会落在那个候选的 LBA1 上。**先清这些，再把 LBA0 交出去**：清掉它们
    // 留下的中间态只是"这处没有 GPT 头"，而先写 LBA0 会在 `at == ss` 的位置造出
    // "新 msdos 表 + 可解析的 GPT 主头"——`GptDamaged`，工具内没有修复入口
    let mut cleared = Vec::new();
    let lba1_whole = src.size >= 2 * ss;
    for at in candidate_sector_sizes(ss) {
        // ss 这一处：目标装得下整扇区 1 时并入下面那次合并写，否则只能单独清签名
        if (at == ss && lba1_whole) || src.size < at + GPT_SIGNATURE.len() as u64 {
            continue;
        }
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        src.read_at(at, &mut sig)?;
        if &sig == GPT_SIGNATURE {
            src.write_at(at, &[0u8; GPT_SIGNATURE.len()])?;
            cleared.push(at);
        }
    }
    // 盘尾备份头也按同一套候选：备份头的位置随表自身的扇区大小而变，只清容器 ss 那一处
    // 会漏掉另一种落盘方式留下的签名。与上面同一顺序要求——先清、后写 LBA0
    for at in candidate_sector_sizes(ss) {
        let off = container_last_lba(src, at).saturating_mul(at);
        if src.size < off.saturating_add(GPT_SIGNATURE.len() as u64) {
            continue;
        }
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        src.read_at(off, &mut sig)?;
        if &sig == GPT_SIGNATURE {
            src.write_at(off, &[0u8; GPT_SIGNATURE.len()])?;
            cleared.push(off);
        }
    }
    // LBA0 与 LBA1 上"不再有 GPT 主头"是同一个事实的两半：分两次写出去，中断正好停在
    // GptDamaged 上。拼进一个缓冲、一次 write，中断只可能停在"旧表"或"新表"
    let mut head = lba0.clone();
    if lba1_whole {
        let mut lba1 = vec![0u8; ss as usize];
        src.read_at(ss, &mut lba1)?;
        if lba1[..GPT_SIGNATURE.len()] == *GPT_SIGNATURE {
            lba1[..GPT_SIGNATURE.len()].fill(0);
            cleared.push(ss);
            head.extend_from_slice(&lba1);
        }
    }
    src.write_at(0, &head)?;
    src.sync_all()?;
    // 读回复核：比的是**意图**（新 LBA0，以及被清掉签名的那些位置），不是刚写下去的
    // 缓冲——写被缓存/镜像层丢掉时，盘会在返回 Ok 之后被判成 GptDamaged
    let mut got = vec![0u8; lba0.len()];
    src.read_at(0, &mut got).map_err(|e| {
        io::Error::new(e.kind(), format!("MBR read-back failed: {e} — the write may not have landed"))
    })?;
    if got != lba0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MBR differs from what was written — the device or image layer dropped the write",
        ));
    }
    for at in cleared {
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        src.read_at(at, &mut sig)?;
        if sig != [0u8; GPT_SIGNATURE.len()] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("GPT signature at byte {at} was not cleared — the device or image layer dropped the write"),
            ));
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
/// 字段（`label` + `damaged`），两者各自独立、互不推导，改这里的措辞不会影响那个契约。
/// 由 `Display` 承载：Rust 里"人读格式化"的惯用表达就是它，出现之处一眼
/// 可辨（`{label}`），不会被人当成可以解析的稳定词汇
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
