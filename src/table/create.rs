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

/// `new`：新建空 GPT（覆盖现有表，破坏表结构但不碰分区数据区）+ 保护 MBR。
/// 几何：128 条目 × 128B，first_usable = 数组之后，last_usable = 末端 - span - 1
/// （由 UEFI 头/数组布局推导的实现约定，非规范逐字给出的公式）。
pub fn create_gpt(src: &mut FileSource, ss: u64, disk_guid: Option<[u8; 16]>) -> io::Result<()> {
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
    ensure_protective_mbr(src)
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

// ---------- msdos（真 MBR）表创建与主分区条目编辑 ----------
// 限制：仅主分区槽位 1..=4，不支持扩展/逻辑分区链。

/// `new --table msdos`：写空 MBR（保留 BootCode，置签名，清空 4 条记录）
pub fn create_mbr(src: &mut FileSource) -> io::Result<()> {
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0)?;
    lba0[446..510].fill(0);
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    src.write_at(0, &lba0)?;
    src.sync_all()
}

pub(crate) fn is_mdos_label(src: &FileSource) -> Result<bool, GptError> {
    Ok(table_label(src)? == TableLabel::Mbr)
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
    if parse_mbr(src)?.is_some() {
        return Ok(TableLabel::Mbr);
    }
    // 这里探到 LBA1 签名而 load_gpt 已放行失败：保护布局不满足的 GPT 盘（保护 MBR
    // 被改写 / LBA0 被抹）。报 `GptDamaged` 而非 `None`——写命令对非 gpt/msdos
    // 标签一律拒绝，且用户看到的是"盘型可辨、布局受损"，不会被诱导用 `new` 覆盖
    // 一份或许还能救回的表；resize 也不会把这种盘当 superfloppy 整盘扩
    if gpt_signature_present(src)? {
        return Ok(TableLabel::GptDamaged);
    }
    Ok(TableLabel::None)
}