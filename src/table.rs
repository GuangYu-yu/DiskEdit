//! 分区表：GPT/MBR 解析 + 崩溃安全写入。
//!
//! 读取经 gptman（主备头自动回退、扇区大小自动探测）。
//! 写入自行序列化（gptman write_into 无 sync、顺序不受控）：
//! 头 92 字节起（实际长度见规范 HeaderSize，CRC 覆盖该长度的字节）+ 条目 128 字节，
//! 均为 UEFI 规范布局；CRC（ISO-HDLC）对实际写盘的字节计算。
//!
//! 写原语的错误类型分两类，判据是调用方要不要区分**未写盘的事前拒绝（退出码 10）**与
//! **写盘后失败（30，须提示"盘可能已改变"）**：
//! - 需要区分 → 返回 `outcome::Fail`：写盘前的校验/形状拒绝写 `Fail::refused`，
//!   I/O 失败交给 `?`（`From<io::Error> for Fail` 落到 `Failed`，即安全缺省）
//! - 不需要（调用方一律按 30 处理）→ 保持 `io::Result`，避免无收益的类型搬运。
//!   例：commit_gpt 的调用方是 table 自己的写原语与写入路径（apply_repair / execute_apply /
//!   execute_resize）

use crate::dev::FileSource;
use crate::outcome::Fail;
use gptman::GPTPartitionEntry;
use std::io;

// ---------- 条目数组几何（canonical，codec 层自持） ----------
//
// "数组有多大"是编解码/布局事实（UEFI 2.10 §5.3.3 Table 5.6：NumberOfPartitionEntries ×
// SizeOfPartitionEntry 决定，不是固定 128 条目），住在本层；操作几何
// （geometry::ValidatedGeometry）只消费它，不让本层反向依赖几何层

/// 自定安全上限（**策略参数，非 UEFI 规范要求**）：条目数组字节数不得超过此值。
/// 刻意不进 `EntryArrayGeometry::new` 的规范判据——由构造点显式传入：解析分配防线与
/// 操作几何共用同一个常量，同一份策略不留第二个副本
pub const MAX_ARRAY_BYTES: u64 = 16 * 1024 * 1024;

/// `new` 造新表时写入的条目数与单条目字节数：建表策略，不是从盘上推导的事实。
/// 新建 GPT 的这两项由实现自选（各工具默认值不同），本工具取 128 × 128B；
/// 改它只影响新造的表，不影响对已有表的解读
pub const DEFAULT_ENTRY_COUNT: u32 = 128;
pub const DEFAULT_ENTRY_SIZE: u32 = 128;

/// 单条目字节数合规判据：UEFI 2.10 §5.3.3 Table 5.6 规定 SizeOfPartitionEntry = 128 × 2^n，
/// 前 128 字节为规范定义字段，其余为保留区（必须为零）
pub fn entry_size_ok(entry_size: u32) -> bool {
    entry_size >= 128 && entry_size.is_power_of_two()
}

fn invalid_geom(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// 条目数组的几何：`sector_size` 是**该表自身**的扇区大小（条目 LBA 的单位），
/// 与容器扇区大小可以不同（4Kn 表放在 512e 容器里），两者不可混用
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryArrayGeometry {
    pub sector_size: u64,
    pub entry_size: u32,
    pub entry_count: u32,
}

impl EntryArrayGeometry {
    /// 唯一构造点：规范判据（扇区大小非零、条目数非零且单条目合规）在此表达；
    /// `max_array_bytes` 是调用方的策略输入（见 [`MAX_ARRAY_BYTES`]），不是本类型的
    /// 规范约束。解析层与写入层共用它，于是"表允许多大"只有一个答案
    pub fn new(sector_size: u64, entry_size: u32, entry_count: u32, max_array_bytes: u64) -> io::Result<Self> {
        if sector_size == 0 {
            return Err(invalid_geom("GPT entry geometry: zero sector size"));
        }
        if entry_count == 0 || !entry_size_ok(entry_size) {
            return Err(invalid_geom("implausible GPT entry geometry"));
        }
        let byte_len = entry_count as u64 * entry_size as u64;
        if byte_len > max_array_bytes {
            return Err(invalid_geom(format!(
                "implausible GPT entry geometry: {entry_count} × {entry_size} B = {byte_len} bytes exceeds the {max_array_bytes}-byte safety limit"
            )));
        }
        Ok(Self { sector_size, entry_size, entry_count })
    }

    /// 数组占用的字节数（UEFI 2.10 §5.3.3 Table 5.6：由两个自述字段相乘决定）
    pub fn byte_len(&self) -> u64 {
        self.entry_count as u64 * self.entry_size as u64
    }

    /// 数组占用的扇区数（向上取整到整扇区）
    pub fn lba_span(&self) -> u64 {
        self.byte_len().div_ceil(self.sector_size)
    }

    /// 1-based 分区号 → 数组下标。越界返回 None：**禁止**用字面量上限直接索引
    pub fn slot(&self, part: u32) -> Option<usize> {
        let idx = part.checked_sub(1)? as usize;
        (idx < self.entry_count as usize).then_some(idx)
    }
}

/// GPT 头签名（UEFI Specification §5.3.2 Table 5.5：Header offset 0、长 8 字节 ASCII）
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
/// 签名 `55 AA`（UEFI §5.2.1 Table 5.1：byte 510 = 0x55、byte 511 = 0xAA）；
/// 本常量是它的 little-endian u16 读法，故写作 0xAA55
const MBR_SIGNATURE: u16 = 0xAA55;
/// 保护 MBR 分区类型：OS Type 0xEE = GPT Protective（UEFI §5.2.2–§5.2.3 Tables 5.2–5.4）
const PROT_MBR_TYPE: u8 = 0xEE;

/// 容器末 LBA（按给定表的扇区大小计）。全仓唯一的算式落点：
/// 不允许在任何调用点重写第二遍——同一事实两个来源迟早分叉。
/// 调用点各有前置（读到扇区 / ≥68 扇区等），`size < ss` 不可达；仍用 saturating
/// 收口而不是裸 `- 1`：release 未开 overflow-checks，下溢会静默回绕成 u64::MAX
///（一个"无限大的容器"），debug 下才 panic——单点算式不该把正确性押在编译模式上
pub(crate) fn container_last_lba(src: &FileSource, ss: u64) -> u64 {
    (src.size / ss).saturating_sub(1)
}

/// 用户可见的"哪一份 GPT 副本"。用于把副本级的损伤讲清楚（主头/主数组坏 vs 备头/备数组坏），
/// 而不是笼统地说"GPT 头坏"
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GptCopyKind {
    Primary,
    Backup,
}

impl GptCopyKind {
    fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Backup => "backup",
        }
    }
}

/// 解析层错误：结构非法的表在类型上不可继续当作正常 GPT 使用（load_gpt 只返回它）。
/// 只提供 `From<io::Error>` 这一个方向——**不提供 `From<GptError> for io::Error`**，
/// 因此"把结构化错误压平"必须经 into_io_error() 在每个调用点显式发生，不会被 `?` 静默吞掉。
#[derive(Debug)]
pub enum GptError {
    /// 底层读写失败
    Io(io::Error),
    /// 头部结构/几何不合法：签名、头尺寸、CRC、MyLBA、usable 上下界关系、条目数组几何
    InvalidHeader(String),
    /// 头部字段越过容器末端（镜像被截断/损坏）
    BeyondContainer {
        field: &'static str,
        value: u64,
        file_last_lba: u64,
    },
    /// 条目自身矛盾：starting_lba > ending_lba（index 为 1-based 分区序号）
    InvalidEntry {
        index: usize,
        start: u64,
        end: u64,
    },
    /// 条目越出 [FirstUsableLBA, LastUsableLBA]（UEFI 2.10 §5.3.1：已定义条目必须落在 usable range 内）
    BeyondUsable {
        index: usize,
        start: u64,
        end: u64,
        first: u64,
        last: u64,
    },
    /// 某一副本的条目数组字节与其头部自述的 CRC 不符：**该副本的数组**是数据损伤。
    /// 单列出来是因为它的可恢复性与前几类不同——另一份副本的数组是独立写入的，
    /// 可能完好，UEFI 2.10 §5.3.2 的主备互备正是为此；是否回退由 load_gpt 决定
    EntryArrayCorrupt { copy: GptCopyKind },
    /// 某一副本的头部字节与自身 CRC 不符（签名在而头不可用）：**该副本的头**是数据损伤。
    /// 与 EntryArrayCorrupt 分列是刻意的：损伤位置不同、对外措辞不同（头坏 vs 数组坏），
    /// 而可恢复性相同——另一份副本的头是独立写入的，可能完好，回退与否仍由 load_gpt 决定
    HeaderCorrupt { copy: GptCopyKind, detail: &'static str },
}

impl From<io::Error> for GptError {
    fn from(e: io::Error) -> Self {
        GptError::Io(e)
    }
}

impl std::fmt::Display for GptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GptError::Io(e) => write!(f, "{e}"),
            GptError::InvalidHeader(s) => f.write_str(s),
            GptError::BeyondContainer { field, value, file_last_lba } => {
                write!(f, "{field} {value} beyond container end (last LBA {file_last_lba}) — truncated image")
            }
            GptError::InvalidEntry { index, start, end } => {
                write!(f, "partition {index}: starting_lba {start} > ending_lba {end} — invalid entry")
            }
            GptError::BeyondUsable { index, start, end, first, last } => {
                write!(f, "partition {index}: {start}..{end} outside usable range {first}..{last}")
            }
            GptError::EntryArrayCorrupt { copy } => {
                write!(f, "{} GPT entry array CRC mismatch", copy.label())
            }
            GptError::HeaderCorrupt { copy, detail } => {
                write!(f, "{} GPT header damaged: {detail}", copy.label())
            }
        }
    }
}

impl std::error::Error for GptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GptError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// 把 GptError 压平为 io::Error：只给"调用方只需知道这次被拒绝"的写入/查询层用。
/// cmd_info 不得使用——它必须直接匹配 GptError 变体做结构化诊断。
pub fn into_io_error(e: GptError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[derive(Clone, Debug)]
pub struct RawHeader {
    pub primary_lba: u64,   // 本头所在 LBA（规范 MyLBA）
    pub backup_lba: u64,    // 对端头 LBA（规范 AlternateLBA）
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: [u8; 16],
    pub partition_entry_lba: u64,
    pub number_of_partition_entries: u32,
    pub size_of_partition_entry: u32,
    /// 规范 HeaderSize：≥ 92 且 ≤ 逻辑块大小，HeaderCRC32 按这么多字节计算
    /// （UEFI 2.10 §5.3.2 Table 5.5）。读到的值随头一起带回，重写时按原值输出——
    /// 头是本工具直接映射的盘上字节，改写不能被固定为 92 而丢掉输入
    pub header_size: u32,
}

#[derive(Clone)]
pub struct RawGpt {
    pub ss: u64,
    pub header: RawHeader,
    pub entries: Vec<GPTPartitionEntry>,
    /// 主头几何状态（UEFI 2.10 §5.3.2：backup header 位于设备最后一个 LBA）
    pub state: GptState,
    /// 保护 MBR 的覆盖范围状态（shape 已满足；UEFI 2.10 §5.2.3）
    pub pmbr: PmbrSize,
}

/// 主头几何状态（UEFI 2.10 §5.3.2：backup header 位于设备最后一个 LBA）。
/// 本枚举只描述**头部这一轴**；保护 MBR 的结论由 PmbrSize 独立表达（正交事实不并入此处）。
/// NeedsRepair = 结构可识别但需写入路径重写头部；该状态不算合法 GPT，
/// info 只报告，由写入路径 repair 修复
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GptState {
    Valid,
    NeedsRepair { cause: HeaderIssue },
}

/// 头部需要重写的原因（互斥，二选一）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderIssue {
    /// 备份头停在旧末端（设备扩容后未搬移）：expected = 容器末端，actual = 头内记录的备份头 LBA
    BackupLbaStale { expected: u64, actual: u64 },
    /// 主副本不可用（头撕裂/清零/CRC 失效，或条目数组 CRC 不符），本表来自盘尾备份副本
    PrimaryUnreadable,
}

/// 保护 MBR 的 SizeInLBA 与当前容器的关系。
/// UEFI 2.10 §5.2.3 把 SizeInLBA 定义为 "disk size minus one"，而 Legacy MBR 的 LBA 字段
/// 一律以设备的 **logical block** 计（不是恒定 512 字节）——故规范值只由
/// `容器大小 / 逻辑块大小` 决定，与 512 字节无关。
///
/// 与 GptState 同构："形状可解析"与"值合乎规范"不共用一个状态。非规范值若被并进 Normal，
/// info 会对外声称该值合法（无任何提示），且 classify_repair 不排修复动作——
/// 兼容值会永久停留，等于把"暂时容忍"变成"永久接受"
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PmbrSize {
    /// 等于规范值（含大盘合法的 0xFFFFFFFF 饱和写法），无需处理
    Normal,
    /// 非规范但可安全重写为规范值，成因见 cause。读取照常放行，写入路径顺手规范化
    NeedsRepair { cause: PmbrIssue },
    /// 大于规范值且不等于已知的 512 字节口径值：可能是更大盘的截断副本，
    /// 按本容器的尺寸重写会抹掉真实布局，只报告、拒绝自动修复
    Inconsistent,
}

/// 保护 MBR 需要修复的两种成因（穷举，不合并）：两者的写入动作相同——都按本容器
/// 重写一次 SizeInLBA——但对外措辞不同，故必须在类型上分开
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PmbrIssue {
    /// 小于规范值：设备扩容后保护 MBR 未随容器更新
    Stale,
    /// 恰等于"按 512 字节扇区算"的值：其他工具在 ss > 512 的盘上按 512 口径写出。
    /// 该值恒大于规范值（ss ≥ 512 ⇒ size/512 ≥ size/ss），按规范重写即收敛
    Compat512,
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

pub fn crc32(data: &[u8]) -> u32 {
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut d = crc.digest();
    d.update(data);
    d.finalize()
}

/// 头部扇区的探测结论。两件事必须分开：候选扇区大小探测要的是"此处有没有 GPT"，
/// 损伤上报要的是"这一份副本的头坏在哪"——压进同一个 Err，签名在而 CRC 坏就会被当成
/// "此处没有 GPT"，一路静默走到 Ok(None)，让后续的 new 覆盖掉或许还能救回的表
enum HeaderProbe {
    /// 签名不匹配：该候选扇区大小下此处没有 GPT
    Absent,
    /// 签名在，但头部自身不可用（header_size 非法 / CRC 不符）：本副本的头字节损伤
    Damaged(&'static str),
    /// 头部可用
    Present(RawHeader),
}

/// 探测 LBA1 的原始头（前 92 字节为规范固定字段，其后至 HeaderSize 为保留区；
/// UEFI 2.10 §5.3.2 Table 5.5 布局），校验签名 + 头 CRC
fn probe_header(sector: &[u8]) -> HeaderProbe {
    if &sector[0..8] != GPT_SIGNATURE {
        return HeaderProbe::Absent;
    }
    let hdr_size = rd_u32(sector, 12) as usize; // signature[8] revision[4] 之后才是 header_size
    if hdr_size < 92 || hdr_size as u64 > sector.len() as u64 {
        return HeaderProbe::Damaged("invalid GPT header size");
    }
    let stored_crc = rd_u32(sector, 16);
    // UEFI 2.10 §5.3.2 Table 5.5：HeaderCRC32 是"把本字段置 0 后对 HeaderSize 字节算的 CRC"，
    // 不是固定 92 字节——同节还写明 HeaderSize ≥ 92、≤ 逻辑块大小，"may increase in the future"。
    // 实盘 HeaderSize 恒为 0x5C(92) 故旧写法兼容性无碍，但对任何合法的更大 HeaderSize 会误判 CRC
    // （上面的 hdr_size 校验已保证 92 ≤ hdr_size ≤ 扇区长度，按它切片不会越界）
    let mut tmp = sector[..hdr_size].to_vec();
    tmp[16..20].fill(0); // CRC 字段置 0 后计算（规范要求）
    if crc32(&tmp) != stored_crc {
        return HeaderProbe::Damaged("GPT header CRC mismatch");
    }
    HeaderProbe::Present(RawHeader {
        primary_lba: rd_u64(sector, 24),
        backup_lba: rd_u64(sector, 32),
        first_usable_lba: rd_u64(sector, 40),
        last_usable_lba: rd_u64(sector, 48),
        disk_guid: sector[56..72].try_into().unwrap(),
        partition_entry_lba: rd_u64(sector, 72),
        number_of_partition_entries: rd_u32(sector, 80),
        size_of_partition_entry: rd_u32(sector, 84),
        header_size: hdr_size as u32,
    })
}

/// GPT 几何校验，依据 UEFI 2.10 §5.3.2 GPT Header：MyLBA = 本头所在 LBA（主头恒为 1）、
/// FirstUsableLBA ≤ LastUsableLBA、LastUsableLBA 是可供分区条目使用的最后 LBA、
/// backup header 位于设备最后一个 LBA；条目数组不得覆盖 LBA0/LBA1，也不得侵入可用区
/// （§5.3.2 规定数组紧跟头部、位于 FirstUsableLBA 之前）。
/// 备份头早于末端 = NeedsRepair（设备扩容后未搬移，由写入路径经 apply_repair 修复）；
/// 越过末端 = 拒绝
///
/// 主/备两个视角的数组位置约束不同，**必须分开**：主数组在可用区**之前**（上界是
/// FirstUsableLBA），备份数组在可用区**之后**（上界是盘尾的备份头）。两条一起写会把
/// 所有合法盘的备份副本误拒
///
/// 调用前提：条目数组几何已由 [`EntryArrayGeometry`] 判定合理（跨度不会溢出）
fn validate_geometry(
    header: &RawHeader,
    geom: &EntryArrayGeometry,
    file_last_lba: u64,
    view: GptCopyKind,
) -> Result<GptState, GptError> {
    let invalid = |m: &str| GptError::InvalidHeader(m.into());
    if header.primary_lba != 1 {
        return Err(invalid("GPT primary_lba != 1 — invalid GPT"));
    }
    if header.first_usable_lba > header.last_usable_lba {
        return Err(invalid("first_usable_lba > last_usable_lba — invalid GPT"));
    }
    if header.last_usable_lba > file_last_lba {
        return Err(GptError::BeyondContainer {
            field: "last_usable_lba",
            value: header.last_usable_lba,
            file_last_lba,
        });
    }
    if header.backup_lba > file_last_lba {
        return Err(GptError::BeyondContainer { field: "backup_lba", value: header.backup_lba, file_last_lba });
    }
    // 数组起点恒须在 LBA0（保护 MBR）与 LBA1（主头）之后——主备两视角同判
    if header.partition_entry_lba < 2 {
        return Err(invalid("partition_entry_lba < 2 — GPT entry array would cover the protective MBR or the header"));
    }
    let span = geom.lba_span();
    let array_end = header
        .partition_entry_lba
        .checked_add(span)
        .ok_or_else(|| invalid("GPT entry array range overflow"))?;
    match view {
        // 主副本：数组夹在头与可用区之间（数组上界不越过 FirstUsableLBA，即
        // FirstUsableLBA ≥ 2 + span；由本条与上面的 LBA ≥ 2 共同推出）
        GptCopyKind::Primary => {
            if array_end > header.first_usable_lba {
                return Err(invalid("GPT entry array overlaps the usable range — invalid GPT"));
            }
        }
        // 备份副本：数组在可用区之后、备份头之前
        GptCopyKind::Backup => {
            if header.partition_entry_lba <= header.last_usable_lba {
                return Err(invalid("backup GPT entry array overlaps the usable range — invalid GPT"));
            }
            if array_end > file_last_lba {
                return Err(invalid("backup GPT entry array beyond the backup header — invalid GPT"));
            }
        }
    }
    Ok(if header.backup_lba == file_last_lba {
        GptState::Valid
    } else {
        GptState::NeedsRepair {
            cause: HeaderIssue::BackupLbaStale { expected: file_last_lba, actual: header.backup_lba },
        }
    })
}

/// 条目级校验（UEFI 2.10 §5.3.1）：已定义条目须 start ≤ end，且整段落在
/// [FirstUsableLBA, LastUsableLBA] 之内。
/// 未使用条目的判据是本工具的规范化分类：两个 LBA 字段同时为零（规范字面为"全字段为零"，
/// 差异只影响"类型 GUID 在、LBA 全零"这一中断态，此处按未使用宽容处理）。
/// 重叠不在此拒绝——那是布局关系 invariant，由各写入路径在生成布局时校验。
fn validate_entries(entries: &[GPTPartitionEntry], header: &RawHeader) -> Result<(), GptError> {
    for (i, e) in entries.iter().enumerate() {
        if e.starting_lba == 0 && e.ending_lba == 0 {
            continue;
        }
        let index = i + 1; // 1-based，与 CLI 的分区序号一致
        if e.starting_lba > e.ending_lba {
            return Err(GptError::InvalidEntry { index, start: e.starting_lba, end: e.ending_lba });
        }
        if e.starting_lba < header.first_usable_lba || e.ending_lba > header.last_usable_lba {
            return Err(GptError::BeyondUsable {
                index,
                start: e.starting_lba,
                end: e.ending_lba,
                first: header.first_usable_lba,
                last: header.last_usable_lba,
            });
        }
    }
    Ok(())
}

/// 单个 GPT 副本的解析结论——只回答"**这一份**能不能用"，不含任何回退决策。
///
/// 与 `GptError` 分开是刻意的：同一个错误码在不同副本上的可恢复性不同，而"要不要换另一份"
/// 是盘级策略。把回退藏进 parse_primary（例如加一个 recover: bool）会让解析与策略重新耦合，
/// 每出现一种新损伤都要在解析函数里改恢复逻辑；改成"解析上报事实、load_gpt 一处定策略"后，
/// 主头坏 / 数组 CRC 坏 / 几何不一致 / 两份都坏，都只在 load_gpt 里加一个分支
///
/// 没有"本副本不可恢复"这一档：两份副本的头与数组是各自独立写入的，任何一份的损伤换个
/// 候选都可能避开——包括读取失败，换候选读的是另一个位置。故解析函数**不可能**让整次
/// load_gpt 提前失败，这一点由返回类型（不带 Err）承担，回退与否全部收敛到 load_gpt
enum ParsedCopy {
    /// 该候选扇区大小下此处没有 GPT
    Absent,
    /// 这一份可用
    Usable(Box<RawGpt>),
    /// 这一份不可用（头或数组的字节损伤、自述几何越界、读取失败）：另一份独立副本可能完好
    CopyDamaged(GptError),
}

/// 头部自述几何的合理性检查 + 条目数组读取 + 数组 CRC 核验（主备两路共用一份）。
/// 四种失败都只涉及**本副本**：几何取自本头的字段、数组按本头的 lba/size 读、
/// CRC 与本头记录的比对、读的是本头指出的那一段字节——换一份副本读的是别处，
/// 故错误由调用方一律归为 CopyDamaged，回退与否留到 load_gpt
fn load_entry_array(
    src: &FileSource,
    sec: &[u8],
    header: &RawHeader,
    ss: u64,
    copy: GptCopyKind,
) -> Result<(Vec<u8>, EntryArrayGeometry), GptError> {
    let bad_geometry = |m: &str| GptError::InvalidHeader(m.into());
    // 条目数组几何的唯一构造点：条目数/单条目大小合规、字节数不超自定安全上限，三件事
    // 都在那里判定，本处不重复表达（此前这里的 16 MiB 上限与 checkpoint 的 128 各说各话）
    let geom = EntryArrayGeometry::new(ss, header.size_of_partition_entry, header.number_of_partition_entries, MAX_ARRAY_BYTES)
        .map_err(|e| bad_geometry(&e.to_string()))?;
    // 损坏表的 lba/size 字段不受信任，乘加全部 checked，防溢出回绕
    let array_off = header.partition_entry_lba.checked_mul(ss).ok_or_else(|| bad_geometry("GPT entry array offset overflow"))?;
    let array_len = geom.byte_len();
    if array_off.checked_add(array_len).ok_or_else(|| bad_geometry("GPT entry array range overflow"))? > src.size {
        return Err(bad_geometry("GPT entry array out of range"));
    }
    let mut raw = vec![0u8; array_len as usize];
    src.read_at(array_off, &mut raw)?;
    if crc32(&raw) != rd_u32(sec, 88) {
        return Err(GptError::EntryArrayCorrupt { copy });
    }
    Ok((raw, geom))
}

/// 每条目取前 128 字节解析（头部自述的 es 可大于 128，余下为保留区）
fn parse_entry_array(raw: &[u8], n: usize, es: usize) -> Vec<GPTPartitionEntry> {
    (0..n).map(|i| parse_entry(&raw[i * es..i * es + 128])).collect()
}

/// 解析主头 + 条目数组（全部自研，写入路径用；保证主头有效）。
/// `pmbr` 由调用方（load_gpt）判定后传入——PMBR 是独立结构，不随扇区候选变化。
/// 返回 Absent = 该扇区大小下 LBA1 无 GPT；返回本副本不可用 = 头/数组损伤、自述几何越界、
/// 或这一处读不出来。任何一种都不阻止 load_gpt 继续试别的候选与盘尾备份
fn parse_primary(src: &FileSource, ss: u64, pmbr: PmbrSize) -> ParsedCopy {
    let mut sec = vec![0u8; ss as usize];
    if src.size < ss * 2 {
        return ParsedCopy::Absent;
    }
    if let Err(e) = src.read_at(ss, &mut sec) {
        // 读不出来是这一处的事（坏扇区），既不是"此处没有 GPT"，也不该就此否掉整块盘
        return ParsedCopy::CopyDamaged(GptError::Io(e));
    }
    let header = match probe_header(&sec) {
        HeaderProbe::Present(h) => h,
        HeaderProbe::Absent => return ParsedCopy::Absent,
        // 签名在而头不可用 = 本副本损伤（另一份可能完好），不是"此处没有 GPT"
        HeaderProbe::Damaged(detail) => {
            return ParsedCopy::CopyDamaged(GptError::HeaderCorrupt { copy: GptCopyKind::Primary, detail })
        }
    };
    let (raw, geom) = match load_entry_array(src, &sec, &header, ss, GptCopyKind::Primary) {
        Ok(r) => r,
        Err(e) => return ParsedCopy::CopyDamaged(e),
    };
    // 几何自洽性校验（validate_geometry）：字段取自本头，末端按本次候选的 ss 口径算，
    // 二者都随副本而变（备份头有它自己的 last_usable / backup_lba），故失败只是本副本不可用
    let state = match validate_geometry(&header, &geom, container_last_lba(src, ss), GptCopyKind::Primary) {
        Ok(s) => s,
        Err(e) => return ParsedCopy::CopyDamaged(e),
    };
    let n = header.number_of_partition_entries as usize;
    let es = header.size_of_partition_entry as usize;
    let entries = parse_entry_array(&raw, n, es);
    // 条目级校验放在数组 CRC 之后：错候选扇区大小已被头 CRC 筛掉，不会误判
    if let Err(e) = validate_entries(&entries, &header) {
        return ParsedCopy::CopyDamaged(e);
    }
    ParsedCopy::Usable(Box::new(RawGpt { ss, header, entries, state, pmbr }))
}

/// 解析备份 GPT（盘尾）。主头不可用（头撕裂/数组损伤/该处读不出来）时的回退路径——
/// 主备互备是 UEFI 2.10 §5.3.2 的规范要求，此刻盘尾备份是唯一能救回分区表的数据。
/// 返回的表规范化为"主头视角"（MyLBA=1 / AltLBA=last_lba），state 置
/// NeedsRepair{PrimaryUnreadable} 以便写入路径 resolve_geometry → apply_repair 重写双头重建主头
fn parse_backup(src: &FileSource, ss: u64, pmbr: PmbrSize) -> ParsedCopy {
    if src.size < ss * 2 {
        return ParsedCopy::Absent;
    }
    let file_last_lba = container_last_lba(src, ss);
    let mut sec = vec![0u8; ss as usize];
    if let Err(e) = src.read_at(file_last_lba * ss, &mut sec) {
        return ParsedCopy::CopyDamaged(GptError::Io(e));
    }
    let mut header = match probe_header(&sec) {
        HeaderProbe::Present(h) => h,
        HeaderProbe::Absent => return ParsedCopy::Absent,
        // 与主头路径同判据：签名在而头不可用是本副本损伤，不是"盘尾没有 GPT"
        HeaderProbe::Damaged(detail) => {
            return ParsedCopy::CopyDamaged(GptError::HeaderCorrupt { copy: GptCopyKind::Backup, detail })
        }
    };
    // 备份头自述：MyLBA = 盘尾、AltLBA = 1（否则不是本盘的备份头）
    if header.primary_lba != file_last_lba || header.backup_lba != 1 {
        return ParsedCopy::Absent;
    }
    let (raw, geom) = match load_entry_array(src, &sec, &header, ss, GptCopyKind::Backup) {
        Ok(r) => r,
        Err(e) => return ParsedCopy::CopyDamaged(e),
    };
    // 转成主头视角后再做几何自洽校验（validate_geometry 按主头语义检查 MyLBA==1）
    header.primary_lba = 1;
    header.backup_lba = file_last_lba;
    if let Err(e) = validate_geometry(&header, &geom, file_last_lba, GptCopyKind::Backup) {
        return ParsedCopy::CopyDamaged(e);
    }
    let n = header.number_of_partition_entries as usize;
    let es = header.size_of_partition_entry as usize;
    let entries = parse_entry_array(&raw, n, es);
    // 与主头路径共用同一份条目校验：只修一条路径等于留洞
    if let Err(e) = validate_entries(&entries, &header) {
        return ParsedCopy::CopyDamaged(e);
    }
    ParsedCopy::Usable(Box::new(RawGpt {
        ss, header, entries,
        state: GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable },
        pmbr,
    }))
}

fn parse_entry(b: &[u8]) -> GPTPartitionEntry {
    let mut name_bytes = [0u8; 72];
    name_bytes.copy_from_slice(&b[56..128]);
    GPTPartitionEntry {
        partition_type_guid: b[0..16].try_into().unwrap(),
        unique_partition_guid: b[16..32].try_into().unwrap(),
        starting_lba: rd_u64(b, 32),
        ending_lba: rd_u64(b, 40),
        attribute_bits: rd_u64(b, 48),
        partition_name: decode_name(&name_bytes),
    }
}

fn decode_name(raw: &[u8; 72]) -> gptman::PartitionName {
    let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes(c.try_into().unwrap())).collect();
    let s: String = String::from_utf16_lossy(&units);
    s.trim_end_matches('\0').into()
}

fn encode_name(entry: &GPTPartitionEntry) -> [u8; 72] {
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
fn serialize_entry(e: &GPTPartitionEntry) -> [u8; 128] {
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
fn serialize_header(h: &RawHeader, array_crc: u32, ss: u64) -> io::Result<Vec<u8>> {
    if h.header_size < 92 || h.header_size as u64 > ss {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GPT HeaderSize {} out of range 92..={ss}", h.header_size),
        ));
    }
    let mut b = vec![0u8; ss as usize];
    b[0..8].copy_from_slice(GPT_SIGNATURE);
    b[8..12].copy_from_slice(&[0x00, 0x00, 0x01, 0x00]); // revision 1.0
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
fn canonical_headers(h: &RawHeader, last_lba: u64, backup_array_lba: u64) -> (RawHeader, RawHeader) {
    let primary = RawHeader {
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
/// 任意落点断电至少存在一份自洽副本且不一致可经 CRC 检出。
pub fn commit_gpt(src: &mut FileSource, g: &RawGpt, last_lba: u64) -> io::Result<()> {
    commit_table(src, g.ss, &g.header, &g.entries, last_lba)
}

/// 崩溃安全四结构序列的唯一实现处：`commit_gpt`（解析侧产物）与
/// [`crate::geometry::ValidatedGeometry::commit`]（写入路径的可操作几何）都走这里，
/// 于是"提交一张表"只有一份序列、一份几何推导
pub(crate) fn commit_table(
    src: &mut FileSource,
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
    let (primary, backup) = canonical_headers(header, last_lba, backup_array_lba);
    // 两份头的字节都在首次写盘之前构造完：构造会因 HeaderSize 越界而失败，
    // 那时盘必须还没被碰过（放到写作序列中间会让拒绝留下半张表）
    let bh = serialize_header(&backup, array_crc, ss)?;
    let ph = serialize_header(&primary, array_crc, ss)?;

    src.write_at(backup_array_lba * ss, &array_bytes)?;
    src.sync_all()?;

    src.write_at(last_lba * ss, &bh)?;
    src.sync_all()?;

    src.write_at(2 * ss, &array_bytes)?;
    src.sync_all()?;

    src.write_at(ss, &ph)?;
    src.sync_all()?;
    Ok(())
}

/// 保护 MBR（LBA0）：保留 BootCode 区 0..446，仅重写 446..512。
/// SizeInLBA = 逻辑块数 − 1（UEFI 2.10 §5.2.3；LBA 字段以 logical block 计，非恒定 512 字节），
/// 超出 32 位表示范围才用 0xFFFFFFFF。本函数同时是规范化点：任何非规范值（stale / 512 口径）
/// 经此重写即收敛
pub fn ensure_protective_mbr(src: &mut FileSource) -> io::Result<()> {
    let ss = src.sector_size;
    // 零扇区盘的 read_at(0) 只会撞出 UnexpectedEof，把"盘没有扇区"这件事
    // 说成一次 I/O 意外：先于任何读盘把几何前提判掉
    let total_sectors = src.size / ss;
    if total_sectors == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "disk has zero sectors"));
    }
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0)?;
    if lba0[510] == 0x55 && lba0[511] == 0xAA {
        let rec = &lba0[446..462]; // 槽位 1（与下方写入位置一致）
        // 与 pmbr_shape_valid 同口径：槽位 2-4 必须全零，否则 hybrid MBR 残留
        // 会使 load_gpt 拒读 GPT（形状前置不满足），盘型判定就此失真
        if lba0[462..510].iter().all(|&b| b == 0) && rec[4] == PROT_MBR_TYPE {
            let start = rd_u32(rec, 8);
            let size = rd_u32(rec, 12);
            let total = src.size / ss;
            // 已合规即一字不写（保留外层引导代码）。比较基准是**逻辑块**口径的 total，
            // 故 512 口径值（PmbrSize::NeedsRepair{Compat512}）在此落空，会走到下面重写为规范值
            if start == 1 && size as u64 + 1 == total.min(u32::MAX as u64 + 1) {
                return Ok(());
            }
        }
    }
    // 保留引导代码，重写 446..512
    lba0[446..510].fill(0);
    let rec = &mut lba0[446..462];
    rec[0] = 0x00; // BootIndicator
    rec[1..4].copy_from_slice(&[0x00, 0x02, 0x00]); // StartCHS 惯例值
    rec[4] = PROT_MBR_TYPE;
    rec[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // EndCHS
    let total_sectors = src.size / ss;
    let size: u32 = if total_sectors > u32::MAX as u64 { u32::MAX } else { (total_sectors - 1) as u32 };
    rec[8..12].copy_from_slice(&1u32.to_le_bytes());
    rec[12..16].copy_from_slice(&size.to_le_bytes());
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    src.write_at(0, &lba0)?;
    src.sync_all()?;
    Ok(())
}

/// MBR（真 MBR 盘）解析：LBA0 @446 起 4×16B，OSIndicator ∈ {0x05,0x0F,0x85} 为容器
#[derive(Debug, Clone)]
pub struct MbrPartition {
    pub num: u32,
    pub os_type: u8,
    pub start_lba: u32,
    pub size_lba: u32,
    pub is_container: bool,
}

/// MBR 的一处损伤。盘型不因它改变（这仍然是一张 MBR 盘），但这份表不能当作可安全
/// 操作的对象——写路径必须拒绝，观察路径必须照实报出
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MbrDamage {
    /// 条目末端越过盘尾（写入侧 `add_mdos_entry` 同样拒绝这种条目）
    PastEnd { num: u32, start: u32, size: u32, total_sectors: u64 },
}

impl MbrDamage {
    pub fn describe(&self) -> String {
        match self {
            MbrDamage::PastEnd { num, start, size, total_sectors } => format!(
                "MBR entry {num} extends past the end of the disk (start {start} + {size} > {total_sectors} sectors)"
            ),
        }
    }
}

/// 原样解析的结果：条目 + 损伤清单。**观察路径消费它**——受损的表仍然是表，`info`
/// 要能看到它（"不可操作"不该连带变成"不可观察"）
#[derive(Debug, Clone)]
pub struct RawMbr {
    pub parts: Vec<MbrPartition>,
    /// 有损伤的条目**仍留在 `parts` 里**：观察者要看的是那条越界的记录本身，
    /// 而不是只知道"表有损伤"
    pub damage: Vec<MbrDamage>,
}

/// 原样解析（不做可操作性判定）。`None` = 这不是一张 MBR 盘
pub fn parse_mbr_raw(src: &FileSource) -> io::Result<Option<RawMbr>> {
    let ss = src.sector_size as usize;
    if src.size < ss as u64 {
        return Ok(None);
    }
    let mut lba0 = vec![0u8; ss];
    src.read_at(0, &mut lba0)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Ok(None);
    }
    // 本工具策略：保护 MBR 布局成立 → 盘交由 GPT 判定（内核探测同口径），不按 msdos 解析。
    // 布局不成立时盘型也不必然是 msdos——0xEE 记录与 LBA1 签名的处置见下方两处
    if pmbr_shape_valid(src)? {
        return Ok(None);
    }
    let mut parts = Vec::new();
    let mut damage = Vec::new();
    let mut saw_protective = false;
    let total_sectors = src.size / ss as u64;
    for i in 0..4u32 {
        let rec = &lba0[446 + (i as usize) * 16..446 + (i as usize) * 16 + 16];
        let os_type = rec[4];
        let start = rd_u32(rec, 8);
        let size = rd_u32(rec, 12);
        if os_type == 0 || size == 0 {
            continue;
        }
        // 0xEE 在 msdos 语义里不是分区（UEFI 2.10 §5.2.3：protective record），
        // 真实 msdos 盘不会合法出现。保护布局受损的 GPT 盘若按槽位解析，
        // 这条会被当成真分区输出，del/flag 随之清除或改写它——毁掉恢复 GPT
        // 所需的保护记录。只排除、不输出；hybrid MBR 的槽位 2-4 是真分区，照常输出。
        // 0xEE 也必须先于越界检查跳过：保护记录的 size 合法值可达 0xFFFFFFFF
        // （min(disk-1, u32::MAX)），小盘上 1+size 必然"越界"，那是 GPT 盘的常态而非损坏
        if os_type == PROT_MBR_TYPE {
            saw_protective = true;
            continue;
        }
        // 越盘条目 = 表已损坏：写入侧有 end >= total 检查，读取侧若放行，info 会照实
        // 打印一个不存在的分区、resize 还会拿这个伪尺寸当基线算目标。这里记为损伤并
        // **保留条目**：可操作性由 `parse_mbr` 拒绝，可观察性由 raw 侧提供
        if start as u64 + size as u64 > total_sectors {
            damage.push(MbrDamage::PastEnd { num: i + 1, start, size, total_sectors });
        }
        parts.push(MbrPartition {
            num: i + 1,
            os_type,
            start_lba: start,
            size_lba: size,
            is_container: matches!(os_type, 0x05 | 0x0F | 0x85),
        });
    }
    // 只剩 0xEE 记录且 LBA1 带 GPT 头签名：盘型是 GPT（LBA1 签名定盘型，保护布局
    // 只定修复分类），不能判成"零分区的 msdos 盘"——否则 del 会把保护记录当空槽清掉。
    // 无签名时（GPT 已灭）只能按 msdos 处置，0xEE 槽已排除，不会误伤残留记录
    if parts.is_empty() && saw_protective && gpt_signature_present(src)? {
        return Ok(None);
    }
    Ok(Some(RawMbr { parts, damage }))
}

/// 校验过的 MBR：与 GPT 侧只接受 `GptState::Valid` 同一口径——有损伤即 Err。
/// 于是所有写路径与依赖表内容的判定，都只可能在一张无损伤的表上进行
pub fn parse_mbr(src: &FileSource) -> io::Result<Option<Vec<MbrPartition>>> {
    let Some(raw) = parse_mbr_raw(src)? else { return Ok(None) };
    if let Some(d) = raw.damage.first() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} — table is damaged", d.describe()),
        ));
    }
    Ok(Some(raw.parts))
}

/// 修改 MBR 主分区条目大小（start 不变，纯扩缩）。LBA 单位与 parse_mbr /
/// add_mdos_entry 的现有约定一致（= 扇区大小）；CHS 字段不动（现代工具惯例，
/// 内核按 LBA 解析）。经 FileSource 写入自动进 undo journal。
///
/// 崩溃语义：普通单扇区写**不是**规范层面的原子写（untorn write 需要设备显式支持，
/// Linux 内核对普通写只作 logical block 原子的行为假设），而 MBR 无 GPT 式 CRC，
/// 掉电撕裂只能靠 0x55AA 签名粗检、字段区损坏不可检出。恢复不依赖"写是原子的"，
/// 依赖的是写前已持久化的 undo journal——发现盘异常后 `undo` 整扇区重写 LBA0 即还原。
pub fn resize_mdos_entry(src: &mut FileSource, part: u32, new_size_lba: u32) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused(format!("invalid MBR partition number {part} (1..4)")));
    }
    let ss = src.sector_size as usize;
    if src.size < ss as u64 {
        return Err(Fail::infra("image too small for MBR"));
    }
    let mut lba0 = vec![0u8; ss];
    // 写盘前的读取：失败即盘未被改动，归 Infra（不能带"盘可能已改变"的提示）
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Err(Fail::infra("no MBR signature on target"));
    }
    let off = 446 + (part as usize - 1) * 16;
    let rec = &mut lba0[off..off + 16];
    if rec[4] == 0 {
        return Err(Fail::refused(format!("MBR partition {part} is empty")));
    }
    if matches!(rec[4], 0x05 | 0x0F | 0x85) {
        return Err(Fail::refused("extended partition container cannot be resized"));
    }
    // 条目自守（与 add_mdos_entry 同严格）：start + 新长度须落在盘内。现有调用方
    // 都先查过 free，但这是 pub 写入口——越界尺寸要挡在写 LBA0 之前，不能指望
    // 每个未来调用方都记得复核
    let start = u32::from_le_bytes(rec[8..12].try_into().unwrap()) as u64;
    let total_sectors = src.size / ss as u64;
    if start == 0 {
        return Err(Fail::refused(format!("MBR partition {part} has invalid start LBA 0")));
    }
    if start + new_size_lba as u64 > total_sectors {
        return Err(Fail::refused(format!(
            "new size {new_size_lba} sectors from LBA {start} exceeds the disk ({total_sectors} sectors)"
        )));
    }
    rec[12..16].copy_from_slice(&new_size_lba.to_le_bytes());
    src.write_at(0, &lba0)?;
    src.sync_all()?;
    Ok(())
}

/// 保护 MBR 形状（UEFI 2.10 §5.2.3）：LBA0 须是有效 MBR（签名 0xAA55），槽位 1 恰为一条
/// 0xEE 记录且 StartingLBA = 1，其余三条记录全为零。
/// 只判形状、不看 SizeInLBA（见 pmbr_size_state）：设备扩容后 SizeInLBA 会过期而形状仍成立。
/// 作为 GPT 判定的前置（内核 GPT 探测同样先要求保护 MBR 形状），用于挡住残留 GPT 头。
fn pmbr_shape_valid(src: &FileSource) -> io::Result<bool> {
    if src.size < 512 {
        return Ok(false);
    }
    let mut lba0 = [0u8; 512];
    src.read_at(0, &mut lba0)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Ok(false);
    }
    for i in 1..4 {
        if lba0[446 + i * 16..446 + i * 16 + 16].iter().any(|&b| b != 0) {
            return Ok(false); // 其余三条记录必须为零
        }
    }
    let rec = &lba0[446..462];
    Ok(rec[4] == PROT_MBR_TYPE && rd_u32(rec, 8) == 1)
}

/// LBA1 是否带 GPT 头签名（UEFI 2.10 §5.3.1：头首 8 字节）。头不自述扇区大小，
/// 与 load_gpt 同一套候选逐个探测。
///
/// 返回 `Err` 表示**读不出来**：那既不能证明有、也不能证明没有，调用方不得当成 `false`
/// ——判成"无签名"会让一张签名读不出的 GPT 盘被当作 msdos 甚至裸盘处置，而写命令会照
/// 那个盘型动手。写路径因此 fail-closed
pub fn gpt_signature_present(src: &FileSource) -> io::Result<bool> {
    for &ss in &candidate_sector_sizes(src.sector_size) {
        if src.size < ss + GPT_SIGNATURE.len() as u64 {
            continue;
        }
        let mut sig = [0u8; GPT_SIGNATURE.len()];
        match src.read_at(ss, &mut sig) {
            Ok(()) if &sig == GPT_SIGNATURE => return Ok(true),
            Ok(()) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// SizeInLBA 与当前容器的关系（唯一判定点）。规范值按设备逻辑块计（UEFI 口径）；
/// 512 口径值单独识别为"非规范但已知"，不并入 Normal——并入就等于宣称它合法。
/// 扇区数超出 32 位表示范围时按规范饱和写 0xFFFFFFFF（此时两种口径期望值相同）
fn pmbr_size_state(src: &FileSource) -> io::Result<PmbrSize> {
    let mut lba0 = [0u8; 512];
    src.read_at(0, &mut lba0)?;
    let size = rd_u32(&lba0, 446 + 12);
    let expect = |total: u64| -> u32 {
        if total > u32::MAX as u64 {
            u32::MAX
        } else {
            (total.saturating_sub(1)) as u32
        }
    };
    let e = expect(src.size / src.sector_size);
    let e512 = expect(src.size / 512);
    if size == e {
        return Ok(PmbrSize::Normal);
    }
    // ss == 512 时两种口径重合，上面已返回 Normal，故此处必有 ss > 512
    if size == e512 {
        return Ok(PmbrSize::NeedsRepair { cause: PmbrIssue::Compat512 });
    }
    // e ≤ e512（扇区大小 ≥ 512），故 size < e 即小于两种口径 → 扩容后的 stale，可安全重写
    Ok(if size < e {
        PmbrSize::NeedsRepair { cause: PmbrIssue::Stale }
    } else {
        PmbrSize::Inconsistent
    })
}

/// 候选扇区大小：容器 ss 优先，再补 512 / 4096。GPT 头不自述扇区大小，镜像与设备的 ss
/// 可能不一致，故逐个探测；容器 ss 已落在候选里时不再重复一次（同一位置同一读法）
fn candidate_sector_sizes(container_ss: u64) -> Vec<u64> {
    let mut v = vec![container_ss];
    for ss in [512, 4096] {
        if ss != container_ss {
            v.push(ss);
        }
    }
    v
}

/// 便捷读取：先要求保护 MBR 形状（否则 LBA1 的残留签名即可骗过判定），再按候选扇区大小
/// （见 candidate_sector_sizes）解析主头，头 CRC 与数组 CRC 均须有效，取首个命中。
///
/// **主备恢复策略只在本函数**（UEFI 2.10 §5.3.2 要求 primary 无效时改用 backup）：
/// - 任一候选、任一份可用 → 用它（来自备份时 state 标记 PrimaryUnreadable，
///   写入路径会重写双头）
/// - 单份不可用（头/条目数组的字节损伤、自述几何越界、这一处读不出来）→ 只是这一份的事：
///   两份副本的头与数组是各自独立写入的，换一份读的是别处字节，故继续试其余候选
/// - 两份都不可用 → 报出首个损伤，绝不因"试过备份"就静默接受损坏的主副本
/// - 两份都没有 GPT 签名 → Ok(None)
pub fn load_gpt(src: &FileSource) -> Result<Option<RawGpt>, GptError> {
    // 形状不满足保护 MBR → 不是 GPT（挡残留 GPT 头）；满足后 SizeInLBA 单独分类，
    // Stale 只标记、不否决（设备扩容后即此形态），交写入路径修复
    if !pmbr_shape_valid(src)? {
        return Ok(None);
    }
    let pmbr = pmbr_size_state(src)?;
    let candidates = candidate_sector_sizes(src.sector_size);
    // 记下首个"本副本不可用"的原因：只有两份副本都给不出可用的表时才需要报它
    let mut damaged: Option<GptError> = None;
    for &ss in &candidates {
        match parse_primary(src, ss, pmbr) {
            ParsedCopy::Usable(g) => return Ok(Some(*g)),
            ParsedCopy::Absent => {}
            ParsedCopy::CopyDamaged(e) => damaged = damaged.or(Some(e)),
        }
    }
    // 主头全部候选都不可用（头撕裂/清零/数组损伤/该处读不出来）：回退解析盘尾备份头。
    // 此刻备份是唯一能救回分区表的数据，不回退会让工具把"主副本坏但备份完好"误判为无表，
    // 进而可能在 new 时覆盖掉这份唯一的副本
    for &ss in &candidates {
        match parse_backup(src, ss, pmbr) {
            ParsedCopy::Usable(g) => return Ok(Some(*g)),
            ParsedCopy::Absent => {}
            ParsedCopy::CopyDamaged(e) => damaged = damaged.or(Some(e)),
        }
    }
    // 两份副本都没有可用的表：有损伤则报出具体原因（结构化，cmd_info 据此出措辞）；
    // 只有两份都没有 GPT 签名才算"无表"——把损伤静默成无表会让后续的 new
    // 覆盖掉或许还能救回的数据
    match damaged {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

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
fn gpt_min_sectors(ss: u64) -> io::Result<u64> {
    // 新建表的条目数与单条目大小取建表策略（[`DEFAULT_ENTRY_COUNT`] /
    // [`DEFAULT_ENTRY_SIZE`]），与盘上任何既有几何无关
    let geom = EntryArrayGeometry::new(ss, DEFAULT_ENTRY_SIZE, DEFAULT_ENTRY_COUNT, MAX_ARRAY_BYTES)?;
    Ok(2 * geom.lba_span() + 4)
}

/// "盘装不下 GPT"的唯一措辞：preflight（命令层写盘前拒绝，Fail::refused）与
/// create_gpt（直调入口的兜底，io::Error）共用同一判据 gpt_min_sectors 与同一句话
fn gpt_too_small(min_sectors: u64, ss: u64) -> String {
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

fn is_mdos_label(src: &FileSource) -> Result<bool, GptError> {
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

/// msdos：主分区条目追加（槽位 1..=4）
pub fn add_mdos_entry(src: &mut FileSource, start: u64, end: u64, os_type: u8) -> Result<u32, Fail> {
    let ss = src.sector_size;
    if !is_mdos_label(src).map_err(|e| Fail::infra(e.to_string()))? {
        return Err(Fail::refused("not an msdos-labelled image"));
    }
    let total = src.size / ss;
    if total == 0 {
        return Err(Fail::infra("image has zero sectors"));
    }
    if start < 1 || end >= total || start > end {
        return Err(Fail::refused(format!("range {start}..{end} outside disk (1..{})", total - 1)));
    }
    // MBR 的 StartLBA/SizeInLBA 都是 u32 字段：超出即无法表示，必须在此拒绝。
    // 少了这一条，下面 `start as u32` / `(end - start + 1) as u32` 会静默截断——
    // 例如 start = 2^32+2048 会写出一个指向 LBA 2048 的条目，落盘即损坏且无任何报错
    if end > u32::MAX as u64 {
        return Err(Fail::refused(format!(
            "range {start}..{end} exceeds the MBR 32-bit LBA limit (last expressible LBA {})",
            u32::MAX
        )));
    }
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    let mut slot: Option<usize> = None;
    for i in 0..4usize {
        let rec = &lba0[446 + i * 16..446 + i * 16 + 16];
        let used = rec[4] != 0 && rd_u32(rec, 12) != 0;
        if !used {
            if slot.is_none() {
                slot = Some(i);
            }
            continue; // 仍需检查后续已用槽位的重叠
        }
        let s = rd_u32(rec, 8) as u64;
        let e = s + rd_u32(rec, 12) as u64 - 1;
        if !(end < s || start > e) {
            return Err(Fail::refused("range overlaps an existing partition"));
        }
    }
    let Some(i) = slot else {
        return Err(Fail::refused("all 4 primary slots used (extended/logical unsupported)"));
    };
    let rec = &mut lba0[446 + i * 16..446 + i * 16 + 16];
    rec[0] = 0x00;
    // CHS 占位写 FE FF FF：工具惯例（LBA-only），非现代 MBR 的必须规则
    rec[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    rec[4] = os_type;
    rec[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    rec[8..12].copy_from_slice(&(start as u32).to_le_bytes());
    rec[12..16].copy_from_slice(&((end - start + 1) as u32).to_le_bytes());
    src.write_at(0, &lba0)?;
    src.sync_all()?;
    Ok((i + 1) as u32)
}

/// msdos：主分区条目删除（清零对应记录）
pub fn del_mdos_entry(src: &mut FileSource, part: u32) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    let rec = &mut lba0[446 + (part as usize - 1) * 16..446 + (part as usize - 1) * 16 + 16];
    if rec[4] == 0 {
        return Err(Fail::refused(format!("partition {part} is already empty")));
    }
    rec.fill(0);
    src.write_at(0, &lba0)?;
    Ok(src.sync_all()?)
}

/// msdos：boot 标志（0x80）开关
pub fn set_mdos_boot(src: &mut FileSource, part: u32, on: bool) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    let off = 446 + (part as usize - 1) * 16;
    if lba0[off + 4] == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    // MBR 规范未规定 boot 标志唯一性，"至多一个"是本工具策略：置位前清掉其他
    if on {
        for i in 0..4usize {
            lba0[446 + i * 16] = 0x00;
        }
    }
    lba0[off] = if on { 0x80 } else { 0x00 };
    src.write_at(0, &lba0)?;
    Ok(src.sync_all()?)
}

/// msdos hidden：OSIndicator 换成 hidden 对应码（visible ↔ visible+0x10），映射取自
/// util-linux pt-mbr-partnames.h；无对应码的类型（如 0x83 Linux）显式拒绝
const MDOS_HIDDEN_PAIRS: &[(u8, u8)] = &[
    (0x01, 0x11), // FAT12
    (0x04, 0x14), // FAT16 <32M
    (0x06, 0x16), // FAT16
    (0x07, 0x17), // HPFS/NTFS/exFAT
    (0x0B, 0x1B), // FAT32
    (0x0C, 0x1C), // FAT32 LBA
    (0x0E, 0x1E), // FAT16 LBA
];

pub fn set_mdos_hidden(src: &mut FileSource, part: u32, on: bool) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    let off = 446 + (part as usize - 1) * 16;
    let cur = lba0[off + 4];
    if cur == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    let want = |t: u8, hide: bool| -> u8 {
        for (v, h) in MDOS_HIDDEN_PAIRS {
            if (hide && *v == t) || (!hide && *h == t) {
                return if hide { *h } else { *v };
            }
        }
        0
    };
    // 已是目标态（cur 本身就是 on 所指的那一侧）即无操作：重复执行同一意图应当
    // 幂等，而不是第二次起被当"无对应类型"拒绝
    if MDOS_HIDDEN_PAIRS.iter().any(|&(v, h)| if on { h == cur } else { v == cur }) {
        return Ok(());
    }
    let next = want(cur, on);
    if next == 0 {
        return Err(Fail::refused(format!(
            "type 0x{cur:02X} has no hidden counterpart (supported: fat12/16/32 incl. lba, ntfs)"
        )));
    }
    lba0[off + 4] = next;
    src.write_at(0, &lba0)?;
    Ok(src.sync_all()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gptman::GPT;
    use std::io::Cursor;

    fn fixture_gpt(ss: u64) -> Vec<u8> {
        let data = vec![0u8; 100 * ss as usize];
        let mut cur = Cursor::new(data);
        let mut gpt = GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
        gpt[1] = GPTPartitionEntry {
            partition_type_guid: [0x01; 16],
            unique_partition_guid: [0x02; 16],
            starting_lba: 34,
            ending_lba: 60,
            attribute_bits: 0,
            partition_name: "test".into(),
        };
        gpt.write_into(&mut cur).unwrap();
        cur.into_inner()
    }

    fn src_from(tag: &str, data: Vec<u8>) -> FileSource {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("diskedit_test_{tag}_{}.img", std::process::id()));
        std::fs::write(&tmp, &data).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
        let size = data.len() as u64;
        FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(&tmp),
            file: f,
            path: tmp,
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
        }
    }

    /// GPT 测试镜像：gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
    fn src_from_gpt(tag: &str, ss: u64) -> FileSource {
        let mut src = src_from(tag, fixture_gpt(ss));
        ensure_protective_mbr(&mut src).unwrap();
        src
    }

    /// 以**原始字节**写入主副本条目（绕过 gptman：它的 write_into 自带 overlap 校验
    /// `InvalidPartitionBoundaries`，而盘上真实存在的重叠表正是别的工具写下的字节，
    /// 解析层必须能读到它）
    fn write_primary_entries(src: &mut FileSource, ss: u64, ents: &[(u64, u64)]) {
        let geom = geom128(ss);
        let parsed: Vec<GPTPartitionEntry> = ents
            .iter()
            .map(|&(s, e)| GPTPartitionEntry {
                partition_type_guid: [0x11; 16],
                unique_partition_guid: [0x22; 16],
                starting_lba: s,
                ending_lba: e,
                attribute_bits: 0,
                partition_name: "".into(),
            })
            .collect();
        let (bytes, crc) = serialize_array(&parsed, &geom);
        src.write_at(2 * ss, &bytes).unwrap();
        let h = geo_header(34, 2048 - geom.lba_span() - 1, 2);
        let sec = serialize_header(&h, crc, ss).unwrap();
        src.write_at(ss, &sec).unwrap();
    }

    /// 重叠条目：诊断层必须仍能读出（info 要能指出哪两条重叠、用户据此修表），
    /// 而写入层必须拒绝——本工具的空间派生事实（右侧空闲、搬移打包、扩容终点）全部
    /// 以"不重叠"为前提（UEFI 2.10 §5.3.1 GPT overview：Each defined partition must not
    /// overlap with any other defined partition）。拒绝发生在任何写盘之前，故归 infra
    #[test]
    fn overlapping_entries_are_diagnosable_but_never_writable() {
        let ss = 512u64;
        let mut src = src_from("overlap", vec![0u8; (2048 * ss) as usize]);
        // 150..250 落在 100..200 内：解析层放行（诊断要看得到），写入层拒绝
        write_primary_entries(&mut src, ss, &[(100, 200), (150, 250), (400, 500)]);
        ensure_protective_mbr(&mut src).unwrap();
        let g = load_gpt(&src)
            .unwrap()
            .expect("a table with overlapping entries must stay observable for diagnostics");
        assert_eq!(crate::geometry::find_overlap(&g.entries), Some((1, 2)));
        let e = crate::gpt_policy::resolve_geometry(&src)
            .err()
            .expect("an overlapping table must be refused by the write-path geometry");
        assert!(matches!(&e, Fail::Infra(m) if m.contains("overlap")), "{e:?}");

        // 紧邻但不重叠（200 / 201）：同一构造点必须放行，不得把合法表一并拒掉
        let mut ok = src_from("no_overlap", vec![0u8; (2048 * ss) as usize]);
        write_primary_entries(&mut ok, ss, &[(100, 200), (201, 250), (400, 500)]);
        ensure_protective_mbr(&mut ok).unwrap();
        assert!(crate::gpt_policy::resolve_geometry(&ok).is_ok());
    }

    /// 容器装不下条目数组时，commit_gpt 必须用 checked 运算返回 Err，
    /// 而不能让 last_lba − span 这类减法下溢（debug 下 panic、release 下回绕）
    #[test]
    fn commit_gpt_refuses_container_too_small() {
        let mut src = src_from_gpt("tiny", 512);
        let g = load_gpt(&src).unwrap().unwrap();
        let e = commit_gpt(&mut src, &g, 0).unwrap_err();
        assert!(e.to_string().contains("too small to hold GPT entry array"), "{e}");
    }

    /// 把 LBA1 主头的 HeaderSize 改成 `hdr_size` 并按新长度重算 CRC；
    /// `reserved` 为真时同时把 92..96 的非零保留字节写进去
    fn patch_primary_header(mut src: FileSource, hdr_size: u32, reserved: bool) -> FileSource {
        let ss = src.sector_size as usize;
        let mut sec = vec![0u8; ss];
        src.read_at(ss as u64, &mut sec).unwrap();
        sec[12..16].copy_from_slice(&hdr_size.to_le_bytes());
        if reserved {
            sec[92..96].fill(0xAA);
        }
        sec[16..20].fill(0);
        let crc = crc32(&sec[..hdr_size as usize]);
        sec[16..20].copy_from_slice(&crc.to_le_bytes());
        src.write_at(ss as u64, &sec).unwrap();
        src
    }

    /// HeaderSize 是规范字段而非固定 92：读到的值必须随头带回并按原值重写，
    /// CRC 覆盖 HeaderSize 字节（含 92 之后的保留区）。写入端固定 92 的实现会把
    /// 合法的更大 HeaderSize 静默规范化掉——读改写不再是恒等变换
    #[test]
    fn header_size_is_preserved_across_rewrite() {
        for (hdr_size, reserved) in [(92u32, false), (96, true), (512, false)] {
            let mut src = patch_primary_header(src_from_gpt(&format!("hsz{hdr_size}"), 512), hdr_size, reserved);
            let g = load_gpt(&src).unwrap().unwrap();
            assert_eq!(g.header.header_size, hdr_size, "parse must carry HeaderSize");
            let last_lba = src.size / 512 - 1;
            commit_gpt(&mut src, &g, last_lba).unwrap();
            let g2 = load_gpt(&src).unwrap().unwrap();
            assert_eq!(g2.header.header_size, hdr_size, "rewrite must keep HeaderSize");
            // 盘上字节自证：字段等于原值，且 CRC 恰覆盖该长度
            let mut sec = vec![0u8; 512];
            src.read_at(512, &mut sec).unwrap();
            assert_eq!(u32::from_le_bytes(sec[12..16].try_into().unwrap()), hdr_size);
            let stored = u32::from_le_bytes(sec[16..20].try_into().unwrap());
            sec[16..20].fill(0);
            assert_eq!(crc32(&sec[..hdr_size as usize]), stored);
        }
    }

    /// 头部自述的 HeaderSize 越出容器/小于规范下限时必须返回错误，
    /// 不得按该值切片（越界 panic）或写出一张 CRC 语义不明的头
    #[test]
    fn header_size_out_of_range_is_an_error() {
        let mut src = src_from_gpt("hszbad", 512);
        let g = load_gpt(&src).unwrap().unwrap();
        let last_lba = src.size / 512 - 1;
        for bad in [0u32, 91, 513, u32::MAX] {
            let mut g = g.clone();
            g.header.header_size = bad;
            let e = commit_gpt(&mut src, &g, last_lba).unwrap_err();
            assert!(e.to_string().contains("HeaderSize"), "{bad}: {e}");
        }
    }

    /// 几何校验的直调夹具：128 × 128B 条目 ⇒ 数组跨度 32 扇区（512B 扇区）
    const SPAN_128_512: u64 = 32;

    fn geo_header(first_usable: u64, last_usable: u64, entry_lba: u64) -> RawHeader {
        RawHeader {
            primary_lba: 1,
            backup_lba: 999,
            first_usable_lba: first_usable,
            last_usable_lba: last_usable,
            disk_guid: [0; 16],
            partition_entry_lba: entry_lba,
            number_of_partition_entries: 128,
            size_of_partition_entry: 128,
            header_size: 92,
        }
    }

    /// 测试用几何：128 条目 × 128B（跨度为 SPAN_128_512），扇区大小由参数给出。
    /// 几何是"表允许多大"的唯一来源，因此测试也不再直接传字面量上限
    fn geom128(ss: u64) -> EntryArrayGeometry {
        EntryArrayGeometry::new(ss, 128, 128, MAX_ARRAY_BYTES).unwrap()
    }

    /// 主副本视角：条目数组必须夹在头部与可用区之间（[2, 2+span) ⊆ 可用区之前）。
    /// 越界的数组会让"分区数据"与"分区表"共用同一段 LBA，读出来的条目是别的字节
    #[test]
    fn geometry_primary_requires_entry_array_before_usable_range() {
        let last = 999;
        let ok = validate_geometry(&geo_header(2 + SPAN_128_512, 900, 2), &geom128(512), last, GptCopyKind::Primary);
        assert!(ok.is_ok(), "spec-shaped table must pass: {ok:?}");
        // 数组起点压住 LBA0/LBA1
        assert!(matches!(
            validate_geometry(&geo_header(34, 900, 1), &geom128(512), last, GptCopyKind::Primary),
            Err(GptError::InvalidHeader(_))
        ));
        // 起点合规但数组上界越过 FirstUsableLBA（比"起点 < 2"隐蔽）
        assert!(matches!(
            validate_geometry(&geo_header(34, 900, 4), &geom128(512), last, GptCopyKind::Primary),
            Err(GptError::InvalidHeader(_))
        ));
        // FirstUsableLBA 装不下数组：first_usable < 2 + span
        assert!(matches!(
            validate_geometry(&geo_header(1, 900, 2), &geom128(512), last, GptCopyKind::Primary),
            Err(GptError::InvalidHeader(_))
        ));
        // 极端下界：拒绝而非回绕/越界
        assert!(matches!(
            validate_geometry(&geo_header(0, 900, 2), &geom128(512), last, GptCopyKind::Primary),
            Err(GptError::InvalidHeader(_))
        ));
    }

    /// 备份副本视角：数组在可用区**之后**、备份头之前。主备两条约束不可混用——
    /// 备份数组本来就不在可用区之前，套用主视角会误拒所有合法盘
    #[test]
    fn geometry_backup_requires_entry_array_after_usable_range() {
        let file_last = 999;
        // 规范形状：数组 [967, 999)，可用区上界 966
        let legal = geo_header(2 + SPAN_128_512, file_last - SPAN_128_512 - 1, file_last - SPAN_128_512);
        assert!(validate_geometry(&legal, &geom128(512), file_last, GptCopyKind::Backup).is_ok());
        // 同一份头按主副本视角必须被拒：证明两视角确实是两套约束
        assert!(validate_geometry(&legal, &geom128(512), file_last, GptCopyKind::Primary).is_err());
        // 数组落进可用区
        assert!(validate_geometry(&geo_header(34, 966, 966), &geom128(512), file_last, GptCopyKind::Backup).is_err());
        // 数组越过备份头（越过盘尾）
        assert!(validate_geometry(&geo_header(34, 966, 968), &geom128(512), file_last, GptCopyKind::Backup).is_err());
        // 起点仍须在 LBA0/LBA1 之后
        assert!(validate_geometry(&geo_header(34, 966, 1), &geom128(512), file_last, GptCopyKind::Backup).is_err());
    }

    /// 头字段判据的拒绝分支：primary_lba、区间倒挂、越容器、数组跨度溢出逐一覆盖
    #[test]
    fn geometry_rejects_malformed_header_fields() {
        let g = geom128(512);
        // primary_lba != 1：UEFI 2.10 §5.3.1 固定主头在 LBA1
        let mut h = geo_header(34, 900, 2);
        h.primary_lba = 2;
        assert!(matches!(validate_geometry(&h, &g, 999, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
        // 区间倒挂
        let h = geo_header(900, 34, 2);
        assert!(matches!(validate_geometry(&h, &g, 999, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
        // last_usable_lba 越容器
        let h = geo_header(34, 1000, 2);
        assert!(matches!(
            validate_geometry(&h, &g, 999, GptCopyKind::Primary),
            Err(GptError::BeyondContainer { field: "last_usable_lba", .. })
        ));
        // backup_lba 越容器
        let mut h = geo_header(34, 900, 2);
        h.backup_lba = 1000;
        assert!(matches!(
            validate_geometry(&h, &g, 999, GptCopyKind::Primary),
            Err(GptError::BeyondContainer { field: "backup_lba", .. })
        ));
        // 数组跨度溢出：checked_add 必须拒绝而非回绕（起点贴着 u64 上界）
        let h = geo_header(34, 900, u64::MAX - 30);
        assert!(matches!(validate_geometry(&h, &g, 999, GptCopyKind::Primary), Err(GptError::InvalidHeader(_))));
    }

    /// 越盘条目：raw 侧保留（info 可观察、可诊断），校验侧 parse_mbr 必须拒绝——
    /// 写入侧有 end >= total 检查，所以坏表只能像真实世界那样来自别处：手写字节绕过
    #[test]
    fn msdos_entry_past_end_is_damaged_and_refused() {
        let mut src = src_from("past_end", vec![0u8; 300 * 512]);
        create_mbr(&mut src).unwrap();
        let mut lba0 = [0u8; 512];
        src.read_at(0, &mut lba0).unwrap();
        let rec = &mut lba0[446..446 + 16];
        rec[4] = 0x0C;
        rec[8..12].copy_from_slice(&1u32.to_le_bytes());
        rec[12..16].copy_from_slice(&10_000u32.to_le_bytes()); // 1 + 10_000 > 300 扇区
        src.write_at(0, &lba0).unwrap();

        let raw = parse_mbr_raw(&src).unwrap().expect("the entry must stay observable for diagnostics");
        assert!(matches!(raw.damage.first(), Some(MbrDamage::PastEnd { .. })), "{:?}", raw.damage);
        let e = parse_mbr(&src).expect_err("a past-end entry must refuse the write path");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// 直接把一份自定义几何的主副本写进镜像（绕过 commit_gpt 的规范化和写入序列）
    fn write_raw_primary(src: &mut FileSource, mut h: RawHeader, ss: u64) {
        let geom = EntryArrayGeometry::new(ss, h.size_of_partition_entry, h.number_of_partition_entries, MAX_ARRAY_BYTES).unwrap();
        let (bytes, crc) = serialize_array(&[], &geom);
        src.write_at(h.partition_entry_lba * ss, &bytes).unwrap();
        h.header_size = 92;
        let sec = serialize_header(&h, crc, ss).unwrap();
        src.write_at(ss, &sec).unwrap();
    }

    /// 端到端：主副本自述 first_usable_lba = 1（装不下条目数组）时必须被拒，
    /// 且不得因"主副本坏"就静默交由别处掩盖
    #[test]
    fn geometry_invariant_reaches_load_gpt() {
        let ss = 512u64;
        let mut src = src_from_gpt("geo_e2e", ss);
        let file_last = src.size / ss - 1;
        // 抹掉盘尾备份头：本用例只考主副本的判定
        src.write_at(file_last * ss, &vec![0u8; ss as usize]).unwrap();
        let mut h = geo_header(1, 66, 2);
        h.backup_lba = file_last;
        write_raw_primary(&mut src, h, ss);
        assert!(matches!(load_gpt(&src), Err(GptError::InvalidHeader(_))));
    }

    /// MBR 的 StartLBA/SizeInLBA 是 u32 字段：超出表示范围必须拒绝。
    /// 少了这条检查，`start as u32` 会静默截断成一个指向别处的条目（落盘即损坏且不报错）
    #[test]
    fn add_mdos_entry_rejects_beyond_32bit_lba() {
        let mut data = vec![0u8; 4096];
        data[510] = 0x55;
        data[511] = 0xAA;
        let mut src = src_from("mdos32", data);
        // 3 TiB 容器：扇区数超过 u32，但 LBA0 只需前 512 字节（真实文件无需那么大）
        src.size = 3 << 40;
        let over = u32::MAX as u64 + 1;
        let e = add_mdos_entry(&mut src, over, over + 2047, 0x83).unwrap_err();
        assert!(matches!(e, Fail::Refused(_)), "must be refused, not truncated: {e:?}");
        // 上限内的值仍应照常写入（新检查不得把合法调用一并拒掉）
        assert_eq!(add_mdos_entry(&mut src, 2048, 4095, 0x83).unwrap(), 1);
    }

    #[test]
    fn entry_size_rules() {
        // UEFI 2.10 §5.3.3 Table 5.6：SizeOfPartitionEntry = 128 × 2^n；192/136 等非法。
        // 判据的唯一实现在 geometry（数组几何的构造点），此处直接消费它
        assert!(entry_size_ok(128));
        assert!(entry_size_ok(256));
        assert!(entry_size_ok(1024));
        assert!(!entry_size_ok(192));
        assert!(!entry_size_ok(136));
        assert!(!entry_size_ok(64));
        // es=256 序列化：前 128 字节为条目，尾部保留区为零，CRC 覆盖整个 n×es
        let e = GPTPartitionEntry {
            partition_type_guid: ESP_TYPE_GUID,
            unique_partition_guid: [0x21; 16],
            starting_lba: 34,
            ending_lba: 100,
            attribute_bits: 0,
            partition_name: "p".into(),
        };
        let (b, crc) = serialize_array(&[e], &EntryArrayGeometry::new(512, 256, 1, MAX_ARRAY_BYTES).unwrap());
        assert_eq!(b.len(), 512);
        assert_eq!(b[128..256], [0u8; 128], "reserved tail must stay zero");
        assert_eq!(crc, crate::table::crc32(&b[..256]));
        // 192 违规：几何构造点即拒绝
        assert!(EntryArrayGeometry::new(512, 192, 1, MAX_ARRAY_BYTES).is_err());
    }

    #[test]
    fn gpt_roundtrip_512() {
        let src = src_from_gpt("g512", 512);
        let g = load_gpt(&src).unwrap().expect("gpt should parse");
        assert_eq!(g.header.number_of_partition_entries, 128);
        assert_eq!(g.header.size_of_partition_entry, 128);
        let used: Vec<_> = g.entries.iter().filter(|e| e.starting_lba != 0 || e.ending_lba != 0).collect();
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].starting_lba, 34);
        assert_eq!(used[0].partition_name.as_str(), "test");
    }

    #[test]
    fn gpt_roundtrip_4kn() {
        let src = src_from_gpt("g4kn", 4096);
        let g = load_gpt(&src).unwrap().expect("gpt should parse at 4096");
        // 4Kn 盘上 128 条目 × 128B = 16 KiB = 4 扇区
        let geom = EntryArrayGeometry::new(4096, g.header.size_of_partition_entry, g.header.number_of_partition_entries, MAX_ARRAY_BYTES).unwrap();
        assert_eq!(geom.lba_span(), 4);
    }

    #[test]
    fn create_gpt_min_capacity_boundaries() {
        // 最小容量 = 2·span+4（512B→68，4Kn→12）；first==last usable 是合法紧边界
        let mut ok = src_from("cg512ok", vec![0u8; 68 * 512]);
        create_gpt(&mut ok, 512, Some([0x11; 16])).unwrap();
        let g = load_gpt(&ok).unwrap().unwrap();
        assert_eq!(g.header.first_usable_lba, 34);
        assert_eq!(g.header.last_usable_lba, 34);
        // gptman 规范侧读回：验证主/备数组在紧边界下不重叠
        let mut cur = Cursor::new(std::fs::read(&ok.path).unwrap());
        GPT::read_from(&mut cur, 512).unwrap();

        let mut bad = src_from("cg512bad", vec![0u8; 67 * 512]);
        assert!(create_gpt(&mut bad, 512, Some([0x11; 16])).is_err());

        let mut ok4 = src_from("cg4knok", vec![0u8; 12 * 4096]);
        create_gpt(&mut ok4, 4096, Some([0x11; 16])).unwrap();
        let g4 = load_gpt(&ok4).unwrap().unwrap();
        assert_eq!(g4.header.first_usable_lba, 6);
        assert_eq!(g4.header.last_usable_lba, 6);

        let mut bad4 = src_from("cg4knbad", vec![0u8; 11 * 4096]);
        assert!(create_gpt(&mut bad4, 4096, Some([0x11; 16])).is_err());
    }

    #[test]
    fn type_guid_constants_are_disk_byte_order() {
        // 内核 include/linux/efi.h EFI_GUID 宏展开的落盘字节（前 3 字段小端）
        assert_eq!(ESP_TYPE_GUID, [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
        assert_eq!(LINUX_FS_TYPE_GUID, [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
    }

    #[test]
    fn corrupt_metadata_refused() {
        // 两例都先补保护 MBR：否则 load_gpt 的前置检查直接返回 None，测不到损坏分支
        // 主头损坏 → 回退盘尾备份头（主备互备），state 标记 Stale 交写入路径重建主头
        let mut data = fixture_gpt(512);
        data[600] ^= 0xFF; // LBA1 头部区域内翻转（破坏 CRC）
        let mut src = src_from("cbad1", data);
        ensure_protective_mbr(&mut src).unwrap();
        let g = load_gpt(&src).unwrap().expect("must fall back to backup header");
        assert!(
            matches!(g.state, GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable }),
            "fallback must be marked for repair"
        );
        assert!(g.entries.iter().any(|e| e.ending_lba != 0), "entries must come from backup array");
        // 主副本的**条目数组**损伤：同样用备份副本救回（UEFI 2.10 §5.3.2 的主备互备），
        // 而不是整体失败。两份数组是分别写入的，这正是回退存在的意义
        let mut data2 = fixture_gpt(512);
        data2[2 * 512 + 10] ^= 0xFF; // LBA2 = 主条目数组
        let mut src2 = src_from("cbad2", data2);
        ensure_protective_mbr(&mut src2).unwrap();
        let g2 = load_gpt(&src2)
            .expect("primary array damage must fall back to the backup")
            .expect("backup copy is intact");
        assert!(
            matches!(g2.state, GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable }),
            "recovered-from-backup must be marked for repair"
        );
        assert!(g2.entries.iter().any(|e| e.ending_lba != 0), "entries must come from backup array");
        // 两份数组都坏 → 最终失败，且报的是"数组损伤"，
        // 绝不能因为"试过备份"就静默接受损坏的主副本
        let mut data3 = fixture_gpt(512);
        data3[2 * 512 + 10] ^= 0xFF; // 主条目数组
        data3[(67 * 512) + 10] ^= 0xFF; // 备条目数组：末块 99 − 跨度 32 = LBA 67
        let mut src3 = src_from("cbad3", data3);
        ensure_protective_mbr(&mut src3).unwrap();
        assert!(matches!(
            load_gpt(&src3),
            Err(GptError::EntryArrayCorrupt { copy: GptCopyKind::Primary })
        ));
    }

    /// 主头签名在而 CRC 坏、且盘尾没有备份：必须报"副本损伤"，不能静默当成"无分区表"。
    /// 后者会让 new 覆盖掉或许还能救回的表——load_gpt 末尾那条设计原则要挡的正是它
    #[test]
    fn damaged_primary_without_backup_is_reported() {
        // 对照组：只抹掉盘尾备份头、头本身完好 → 仍是可用的表。
        // 有它才能证明下面那个 Err 来自头损伤，而不是"没有备份"本身
        let mut intact = fixture_gpt(512);
        let last = intact.len() - 512;
        intact[last..].fill(0); // 备份头所在扇区清零
        let mut ctl = src_from("hdmg_ctl", intact);
        ensure_protective_mbr(&mut ctl).unwrap();
        assert!(load_gpt(&ctl).unwrap().is_some(), "对照组：头完好时缺备份不影响解析");

        let mut data = fixture_gpt(512);
        let last = data.len() - 512;
        data[last..].fill(0);
        data[512 + 16] ^= 0xFF; // LBA1 头部 CRC 字段内翻转：签名完好，CRC 对不上
        let mut src = src_from("hdmg", data);
        ensure_protective_mbr(&mut src).unwrap();
        assert!(
            matches!(load_gpt(&src), Err(GptError::HeaderCorrupt { copy: GptCopyKind::Primary, .. })),
            "签名在而头 CRC 坏且无备份 ⇒ 必须报 HeaderCorrupt(primary)"
        );
    }

    /// 读不出来只是"这一处"的事：主头那一段读失败时，盘尾备份照旧可用。
    /// 让局部读取失败否掉整块盘，等于一次坏扇区就丢掉唯一能救回的表
    #[test]
    fn unreadable_primary_sector_falls_back_to_backup() {
        let src = src_from_gpt("iofb", 512);
        {
            let _fault = crate::dev::ReadFaultGuard::at(512); // LBA1；盘尾备份在另一处偏移
            let g = load_gpt(&src).expect("a bad sector must not fail the whole load").expect("the backup copy is intact");
            assert!(
                matches!(g.state, GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable }),
                "recovered-from-backup must be marked for repair: {:?}",
                g.state
            );
            assert!(g.entries.iter().any(|e| e.ending_lba != 0), "entries must come from the backup array");
        }
        // 守卫析构即撤掉注入：同一处再读正常，认的是主副本
        assert!(load_gpt(&src).unwrap().is_some(), "the fault must not outlive its guard");
    }

    /// 两份副本的**头**都不可用 ⇒ 报错，不能静默成"无表"——后者会让 new 覆盖掉或许
    /// 还能救回的表。与 damaged_primary_without_backup_is_reported 成对：
    /// 那条测的是单份损伤走回退，这条测的是两份都损伤必须浮出
    #[test]
    fn both_headers_unusable_is_an_error_not_absence() {
        let mut data = fixture_gpt(512);
        let last = data.len() - 512;
        data[512 + 16] ^= 0xFF; // 主头 CRC 字段：签名在，CRC 对不上
        data[last + 16] ^= 0xFF; // 备头同样处理
        let mut src = src_from("bothhdr", data);
        ensure_protective_mbr(&mut src).unwrap();
        assert!(
            matches!(load_gpt(&src), Err(GptError::HeaderCorrupt { copy: GptCopyKind::Primary, .. })),
            "两份都坏必须报错，且报首个损伤"
        );
    }

    /// 两份副本都没有 GPT 签名（空盘）⇒ Ok(None)。与上一条成对：损伤与"没有表"必须区分开
    #[test]
    fn no_gpt_signature_is_absence() {
        let mut src = src_from("nogpt", vec![0u8; 100 * 512]);
        ensure_protective_mbr(&mut src).unwrap();
        assert!(pmbr_shape_valid(&src).unwrap(), "前提：形状成立才会走到副本解析");
        assert!(load_gpt(&src).unwrap().is_none(), "无签名 ⇒ 无表，不是损伤");
    }

    #[test]
    fn msdos_hidden_toggle() {
        let data = vec![0u8; 300 * 512];
        let mut src = src_from("mhidden", data);
        create_mbr(&mut src).unwrap();
        add_mdos_entry(&mut src, 63, 200, 0x0B).unwrap();
        set_mdos_hidden(&mut src, 1, true).unwrap();
        assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x1B);
        set_mdos_hidden(&mut src, 1, false).unwrap();
        assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x0B);
        // 无 hidden 对应码的类型拒绝
        add_mdos_entry(&mut src, 210, 250, 0x83).unwrap();
        assert!(set_mdos_hidden(&mut src, 2, true).is_err());
    }

    /// resize_mdos_entry 是 pub 写入口：start + 新长度越出盘尾必须拒绝——
    /// 与 add_mdos_entry 的自守同严格，不能指望每个调用方都先查过 free
    #[test]
    fn resize_mdos_entry_rejects_beyond_disk_end() {
        let data = vec![0u8; 300 * 512];
        let mut src = src_from("mrszend", data);
        create_mbr(&mut src).unwrap();
        add_mdos_entry(&mut src, 63, 200, 0x83).unwrap();
        // 63 + 300 > 300 盘尾：拒绝且不落盘
        let e = resize_mdos_entry(&mut src, 1, 300).unwrap_err();
        assert!(matches!(e, Fail::Refused(_)), "must be refused: {e:?}");
        // 仍在盘内的扩容照常写入（新检查不得把合法调用一并拒掉）
        resize_mdos_entry(&mut src, 1, 200).unwrap();
        assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].size_lba, 200);
    }

    /// 保护 MBR（UEFI 2.10 §5.2.3）：形状与 SizeInLBA 覆盖范围分层判定
    #[test]
    fn protective_mbr_shape_and_size() {
        let mut src = src_from_gpt("pmbr", 512); // 100 扇区 → SizeInLBA = 99
        assert!(pmbr_shape_valid(&src).unwrap());
        assert_eq!(pmbr_size_state(&src).unwrap(), PmbrSize::Normal);
        let good = {
            let mut b = [0u8; 512];
            src.read_at(0, &mut b).unwrap();
            b
        };
        // 每条用例：在有效布局上只改一处字节
        let patch = |src: &mut FileSource, f: &dyn Fn(&mut [u8; 512])| {
            let mut b = good;
            f(&mut b);
            src.write_at(0, &b).unwrap();
        };
        // 形状：签名丢失 / 类型非 0xEE / StartingLBA != 1 / 其余三条记录非零
        patch(&mut src, &|b| b[510] = 0);
        assert!(!pmbr_shape_valid(&src).unwrap());
        patch(&mut src, &|b| b[446 + 4] = 0x83);
        assert!(!pmbr_shape_valid(&src).unwrap());
        patch(&mut src, &|b| b[446 + 8..446 + 12].copy_from_slice(&2u32.to_le_bytes()));
        assert!(!pmbr_shape_valid(&src).unwrap());
        patch(&mut src, &|b| b[446 + 16 + 4] = 0x0B);
        assert!(!pmbr_shape_valid(&src).unwrap());

        // 尺寸：形状成立与否不受 SizeInLBA 影响，尺寸单独分类
        patch(&mut src, &|_| {});
        assert!(pmbr_shape_valid(&src).unwrap());
        // 小于容器（99 → 50）= 扩容后的 stale，形状仍有效
        patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&50u32.to_le_bytes()));
        assert!(pmbr_shape_valid(&src).unwrap());
        assert_eq!(
            pmbr_size_state(&src).unwrap(),
            PmbrSize::NeedsRepair { cause: PmbrIssue::Stale }
        );
        // 等于容器 = Normal；大于容器且非已知口径 = 不一致（拒绝自动修复）；0 亦视为 stale
        patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&99u32.to_le_bytes()));
        assert_eq!(pmbr_size_state(&src).unwrap(), PmbrSize::Normal);
        patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&200u32.to_le_bytes()));
        assert_eq!(pmbr_size_state(&src).unwrap(), PmbrSize::Inconsistent);
        patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&0u32.to_le_bytes()));
        assert_eq!(
            pmbr_size_state(&src).unwrap(),
            PmbrSize::NeedsRepair { cause: PmbrIssue::Stale }
        );

        // 大盘（4 TiB，32 位表示不下）：按规范饱和写 0xFFFFFFFF → Normal
        let mut big = src_from("pmbr_big", vec![0u8; 8192]);
        big.size = 4 * 1024 * 1024 * 1024 * 1024u64;
        {
            let mut b = [0u8; 512];
            big.read_at(0, &mut b).unwrap();
            b[446 + 4] = 0xEE;
            b[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
            b[446 + 12..446 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
            b[510] = 0x55;
            b[511] = 0xAA;
            big.write_at(0, &b).unwrap();
        }
        assert!(pmbr_shape_valid(&big).unwrap());
        assert_eq!(pmbr_size_state(&big).unwrap(), PmbrSize::Normal);

        // 4Kn 容器上的 512 字节口径值：UEFI 2.10 §5.2.3 的 SizeInLBA 以 logical block 计，
        // 512 口径是别的工具的历史写法 → 判为"非规范但可修复"，不得并进 Normal
        // （并入的后果：info 声称它合法，且 classify_repair 不排修复动作，兼容值永久停留）
        let mut k4 = src_from_gpt("pmbr_4kn", 4096); // 100 块 × 4096 = 409600 字节
        k4.sector_size = 4096; // 规范值 = 100 − 1 = 99；512 口径值 = 409600/512 − 1 = 799
        let set_size = |src: &mut FileSource, v: u32| {
            let mut b = vec![0u8; src.sector_size as usize];
            src.read_at(0, &mut b).unwrap();
            b[446 + 4] = 0xEE;
            b[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
            b[446 + 12..446 + 16].copy_from_slice(&v.to_le_bytes());
            b[510] = 0x55;
            b[511] = 0xAA;
            src.write_at(0, &b).unwrap();
        };
        set_size(&mut k4, 799);
        assert!(pmbr_shape_valid(&k4).unwrap());
        assert_eq!(
            pmbr_size_state(&k4).unwrap(),
            PmbrSize::NeedsRepair { cause: PmbrIssue::Compat512 }
        );
        // 策略层必须把它排进修复动作，否则写入路径不会规范化
        let last = k4.size / 4096 - 1;
        let g4 = load_gpt(&k4).unwrap().unwrap();
        assert_eq!(
            crate::gpt_policy::classify_repair(&g4, last).unwrap(),
            crate::gpt_policy::RepairAction::RepairProtectiveMbr
        );
        // 规范化点：写入路径重写一次即收敛
        ensure_protective_mbr(&mut k4).unwrap();
        assert_eq!(pmbr_size_state(&k4).unwrap(), PmbrSize::Normal);
        // 大于规范值且非已知口径（如 800）→ 可能是更大盘的截断副本，仍拒绝自动修复
        set_size(&mut k4, 800);
        assert_eq!(pmbr_size_state(&k4).unwrap(), PmbrSize::Inconsistent);
        let g4 = load_gpt(&k4).unwrap().unwrap();
        assert!(crate::gpt_policy::classify_repair(&g4, last).is_err());
    }

    /// 盘型由 LBA1 签名判定（UEFI 2.10 §5.3），保护 MBR 布局只定修复分类：
    /// 布局破损的 GPT 盘绝不能按 msdos 解析。0xEE 槽若被当成真分区输出，
    /// del/flag 会清除或改写它，恢复 GPT 所需的保护记录就此毁掉
    #[test]
    fn damaged_protective_mbr_never_parses_as_msdos() {
        let mut src = src_from_gpt("pmbdmg", 512);
        let mut good = [0u8; 512];
        src.read_at(0, &mut good).unwrap();

        // 破法一：槽位 1 仍是 0xEE 但 StartingLBA 被改坏——签名在 ⇒ 盘型 GPT，不判 msdos
        let mut b = good;
        b[446 + 8..446 + 12].copy_from_slice(&2u32.to_le_bytes());
        src.write_at(0, &b).unwrap();
        assert!(!pmbr_shape_valid(&src).unwrap());
        assert!(parse_mbr(&src).unwrap().is_none(), "0xEE 槽不是分区，签名在 ⇒ 不判 msdos");
        assert_eq!(table_label(&src).unwrap(), TableLabel::GptDamaged);

        // 破法二：槽位 1 类型被改写成真实分区类型——内核同口径判 msdos（真实槽位）
        let mut b = good;
        b[446 + 4] = 0x0B;
        src.write_at(0, &b).unwrap();
        assert_eq!(table_label(&src).unwrap(), TableLabel::Mbr);
        let m = parse_mbr(&src).unwrap().unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].os_type, 0x0B);

        // 破法三：签名抹掉（GPT 全灭）+ 槽位 1 起点被改坏——无签名无从判 GPT，
        // 0xEE 槽已排除，等价于零分区的 msdos 盘
        let mut b = good;
        b[446 + 8..446 + 12].copy_from_slice(&2u32.to_le_bytes());
        src.write_at(0, &b).unwrap();
        src.write_at(512, &vec![0u8; 512]).unwrap();
        assert!(!pmbr_shape_valid(&src).unwrap());
        assert!(parse_mbr(&src).unwrap().unwrap().is_empty());
        assert_eq!(table_label(&src).unwrap(), TableLabel::Mbr);

        // hybrid 布局：0xEE + 真实槽位 → 只输出真实槽位（内核同口径按 msdos 管）
        let mut h = src_from("hybrid", vec![0u8; 300 * 512]);
        create_mbr(&mut h).unwrap();
        add_mdos_entry(&mut h, 63, 200, 0x0B).unwrap();
        add_mdos_entry(&mut h, 210, 250, 0x83).unwrap(); // 第二分区进槽位 2
        let mut lba0 = [0u8; 512];
        h.read_at(0, &mut lba0).unwrap();
        lba0[446 + 4] = 0xEE;
        h.write_at(0, &lba0).unwrap();
        // msdos 盘上 LBA1 的残留 GPT 头：槽位是真实分区 → 照常按 msdos 管
        let mut sig = vec![0u8; 512];
        sig[..8].copy_from_slice(GPT_SIGNATURE);
        h.write_at(512, &sig).unwrap();
        let m = parse_mbr(&h).unwrap().unwrap();
        assert_eq!(m.len(), 1, "0xEE 槽不输出");
        assert_eq!(m[0].num, 2);
        assert_eq!(table_label(&h).unwrap(), TableLabel::Mbr);
    }

    /// 条目数组：128 × 2^n 合规，其余拒绝；字节数上限是策略参数（非规范），
    /// 超限即拒绝而不是按字段做巨型分配
    #[test]
    fn entry_array_geometry_bounds() {
        assert!(EntryArrayGeometry::new(512, 128, 128, MAX_ARRAY_BYTES).is_ok());
        assert!(EntryArrayGeometry::new(4096, 128, 128, MAX_ARRAY_BYTES).is_ok());
        assert!(EntryArrayGeometry::new(512, 256, 128, MAX_ARRAY_BYTES).is_ok());
        // 条目数为 0 / 单条目不合规 / 扇区大小为 0
        assert!(EntryArrayGeometry::new(512, 128, 0, MAX_ARRAY_BYTES).is_err());
        assert!(EntryArrayGeometry::new(512, 192, 128, MAX_ARRAY_BYTES).is_err());
        assert!(EntryArrayGeometry::new(0, 128, 128, MAX_ARRAY_BYTES).is_err());
        // 16 MiB 上限：128B 条目 ⇒ 上界 131072 条；再大一档即拒绝。
        // 上限是调用方的策略输入：同一个"越限"几何换个宽松上限就合法
        assert!(EntryArrayGeometry::new(512, 128, 131_072, MAX_ARRAY_BYTES).is_ok());
        assert!(EntryArrayGeometry::new(512, 128, 262_144, MAX_ARRAY_BYTES).is_err());
        assert!(EntryArrayGeometry::new(512, 128, 262_144, u64::MAX).is_ok());
    }

    /// 跨度按表自身的扇区大小算：128 × 128B 在 512B 下 32 扇区、在 4Kn 下 4 扇区
    #[test]
    fn lba_span_follows_table_sector_size() {
        assert_eq!(EntryArrayGeometry::new(512, 128, 128, MAX_ARRAY_BYTES).unwrap().lba_span(), 32);
        assert_eq!(EntryArrayGeometry::new(4096, 128, 128, MAX_ARRAY_BYTES).unwrap().lba_span(), 4);
        // 256 条目 × 256B = 64 KiB ⇒ 4Kn 下 16 扇区
        assert_eq!(EntryArrayGeometry::new(4096, 256, 256, MAX_ARRAY_BYTES).unwrap().lba_span(), 16);
    }

    /// 槽位换算：分区号上界来自本表的条目数，128 不是硬上限
    #[test]
    fn slot_is_derived_from_entry_count() {
        let g = EntryArrayGeometry::new(512, 128, 256, MAX_ARRAY_BYTES).unwrap();
        assert_eq!(g.slot(1), Some(0));
        assert_eq!(g.slot(256), Some(255));
        assert_eq!(g.slot(257), None);
        assert_eq!(g.slot(0), None);
    }

    /// msdos hidden 的幂等：已是目标态时重复同一意图必须成功且不改字节。第二次起被
    /// 当"无对应类型"拒绝是这类"读当前值再决定"的改写的典型回归
    #[test]
    fn mdos_hidden_is_idempotent() {
        let mut src = src_from("hidden_idem", vec![0u8; 300 * 512]);
        create_mbr(&mut src).unwrap();
        add_mdos_entry(&mut src, 63, 200, 0x0C).unwrap(); // FAT32 LBA（可见侧）
        let ty = |src: &FileSource| {
            let mut lba0 = [0u8; 512];
            src.read_at(0, &mut lba0).unwrap();
            lba0[446 + 4]
        };

        set_mdos_hidden(&mut src, 1, true).unwrap();
        assert_eq!(ty(&src), 0x1C);
        set_mdos_hidden(&mut src, 1, true).unwrap();
        assert_eq!(ty(&src), 0x1C, "a repeated hide must be a no-op, not a refusal");

        set_mdos_hidden(&mut src, 1, false).unwrap();
        assert_eq!(ty(&src), 0x0C);
        set_mdos_hidden(&mut src, 1, false).unwrap();
        assert_eq!(ty(&src), 0x0C, "a repeated unhide must be a no-op, not a refusal");
    }

    /// 非配对类型两个方向都拒绝：幂等短路只认配对表内的值，不得把"未知类型"当成
    /// "已在目标态"
    #[test]
    fn mdos_hidden_rejects_unpaired_type() {
        let mut src = src_from("hidden_unpaired", vec![0u8; 300 * 512]);
        create_mbr(&mut src).unwrap();
        add_mdos_entry(&mut src, 63, 200, 0x83).unwrap(); // Linux data：无 hidden 对应码
        let e = set_mdos_hidden(&mut src, 1, true).expect_err("0x83 must be refused");
        assert!(matches!(&e, Fail::Refused(m) if m.contains("no hidden counterpart")), "{e:?}");
        assert!(set_mdos_hidden(&mut src, 1, false).is_err(), "0x83 must be refused in both directions");
    }

    /// 条目级结构校验（UEFI 2.10 §5.3.1）：倒挂 / 越出 usable range → 结构化错误；
    /// 两个 LBA 字段同时为零按未使用放行（本工具的规范化分类）
    #[test]
    fn entry_geometry_validation() {
        let mut src = src_from_gpt("eval", 512);
        let mut g = load_gpt(&src).unwrap().unwrap();
        let first = g.header.first_usable_lba;
        let last_usable = g.header.last_usable_lba;
        let last = src.size / 512 - 1;
        assert!(last_usable < last, "fixture must leave space beyond usable range");
        let set = |g: &mut RawGpt, s: u64, e: u64| {
            g.entries[0] = GPTPartitionEntry {
                partition_type_guid: LINUX_FS_TYPE_GUID,
                unique_partition_guid: [0x77; 16],
                starting_lba: s,
                ending_lba: e,
                attribute_bits: 0,
                partition_name: "bad".into(),
            };
        };
        // 倒挂：start > end
        set(&mut g, 100, 50);
        commit_gpt(&mut src, &g, last).unwrap();
        assert!(matches!(
            load_gpt(&src),
            Err(GptError::InvalidEntry { index: 1, start: 100, end: 50 })
        ));
        // end == 0 且 start != 0：同属倒挂，不是"未使用"
        set(&mut g, 100, 0);
        commit_gpt(&mut src, &g, last).unwrap();
        assert!(matches!(load_gpt(&src), Err(GptError::InvalidEntry { index: 1, .. })));
        // 越出 usable 上界（实体仍在容器内，故不是 BeyondContainer）
        set(&mut g, first, last_usable + 1);
        commit_gpt(&mut src, &g, last).unwrap();
        assert!(matches!(load_gpt(&src), Err(GptError::BeyondUsable { index: 1, .. })));
        // 低于 usable 下界
        set(&mut g, first - 1, first + 5);
        commit_gpt(&mut src, &g, last).unwrap();
        assert!(matches!(load_gpt(&src), Err(GptError::BeyondUsable { index: 1, .. })));
        // 两字段同时为零 → 未使用（类型 GUID 残留也宽容）
        set(&mut g, 0, 0);
        commit_gpt(&mut src, &g, last).unwrap();
        let g2 = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g2.entries[0].starting_lba, 0);
        // 规范化分类的直接推论：存活条目里 end == 0 ⟺ start == 0
        assert!(g2.entries.iter().all(|e| (e.ending_lba == 0) == (e.starting_lba == 0)));
    }

    #[test]
    fn commit_roundtrip_via_gptman() {
        // 写入后用 gptman 读回，验证序列化与 CRC 符合规范
        let mut src = src_from_gpt("gcommit", 512);
        let mut g = load_gpt(&src).unwrap().unwrap();
        g.entries[2] = GPTPartitionEntry {
            partition_type_guid: [0x03; 16],
            unique_partition_guid: [0x04; 16],
            starting_lba: 64,
            ending_lba: 70,
            attribute_bits: 0,
            partition_name: "second".into(),
        };
        let last_lba = src.size / 512 - 1;
        commit_gpt(&mut src, &g, last_lba).unwrap();
        // gptman 读回即规范侧验收（gptman 索引 1 起：g.entries[2] 为 partition #3）
        let mut f = src.file.try_clone().unwrap();
        let gpt = GPT::find_from(&mut f).expect("gptman must accept our serialization");
        let p = &gpt[3];
        assert_eq!(p.starting_lba, 64);
        assert_eq!(p.partition_name.as_str(), "second");
        // 保护 MBR 幂等
        ensure_protective_mbr(&mut src).unwrap();
        ensure_protective_mbr(&mut src).unwrap();
    }
}