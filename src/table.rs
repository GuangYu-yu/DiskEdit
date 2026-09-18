//! 分区表：GPT/MBR 解析 + 崩溃安全写入。
//!
//! 读取经 gptman（主备头自动回退、扇区大小自动探测）。
//! 写入自行序列化（gptman write_into 无 sync、顺序不受控）：
//! 头 92 字节 + 条目 128 字节，均为 UEFI 规范布局；CRC（ISO-HDLC）
//! 对实际写盘的字节计算。
//!
//! 写原语的错误类型分两类，判据是调用方要不要区分**未写盘的事前拒绝（退出码 10）**与
//! **写盘后失败（30，须提示"盘可能已改变"）**：
//! - 需要区分 → 返回 `outcome::Fail`：写盘前的校验/形状拒绝写 `Fail::refused`，
//!   I/O 失败交给 `?`（`From<io::Error> for Fail` 落到 `Failed`，即安全缺省）
//! - 不需要（调用方一律按 30 处理，如 commit_gpt 只被 ensure_geometry/apply_inner 使用）
//!   → 保持 `io::Result`，避免无收益的类型搬运

use crate::dev::FileSource;
use crate::outcome::Fail;
use gptman::GPTPartitionEntry;
use std::io;

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const MBR_SIGNATURE: u16 = 0xAA55;
/// 保护 MBR 分区类型
const PROT_MBR_TYPE: u8 = 0xEE;

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
/// 因此"把结构化错误压平"必须经 flatten() 在每个调用点显式发生，不会被 `?` 静默吞掉。
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
pub fn flatten(e: GptError) -> io::Error {
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

/// 探测 LBA1 的原始头（92 字节，UEFI 2.10 §5.3.2 Table 5.5 布局），校验签名 + 头 CRC
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
    })
}

/// GPT 几何校验，依据 UEFI 2.10 §5.3.2 GPT Header：MyLBA = 本头所在 LBA（主头恒为 1）、
/// FirstUsableLBA ≤ LastUsableLBA、LastUsableLBA 是可供分区条目使用的最后 LBA、
/// backup header 位于设备最后一个 LBA。
/// 备份头早于末端 = NeedsRepair（设备扩容后未搬移，由写入路径经 ensure_geometry repair 修复）；
/// 越过末端 = 拒绝
fn validate_geometry(header: &RawHeader, file_last_lba: u64) -> Result<GptState, GptError> {
    if header.primary_lba != 1 {
        return Err(GptError::InvalidHeader("GPT primary_lba != 1 — invalid GPT".into()));
    }
    if header.first_usable_lba > header.last_usable_lba {
        return Err(GptError::InvalidHeader("first_usable_lba > last_usable_lba — invalid GPT".into()));
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
enum ParsedCopy {
    /// 该候选扇区大小下此处没有 GPT
    Absent,
    /// 这一份可用
    Usable(Box<RawGpt>),
    /// 这一份的**头或数组字节**损伤（CRC 不符）：本副本不可用，但另一份独立副本可能完好
    CopyDamaged(GptError),
    /// 不可恢复：头部自述几何不合规、数组越出容器、条目语义非法、读取失败。
    /// 这些是盘/容器层面的前提，另一份副本同样不满足，换副本无解
    Fatal(GptError),
}

/// 头部自述几何的合理性检查 + 条目数组读取 + 数组 CRC 核验（主备两路共用一份）。
/// CRC 不符 = 本副本的数组字节损伤，另一份独立副本可能完好 → CopyDamaged；
/// 几何异常 / 数组越出容器是盘与容器层面的前提，换副本同样不满足 → Fatal
fn load_entry_array(
    src: &FileSource,
    sec: &[u8],
    header: &RawHeader,
    ss: u64,
    copy: GptCopyKind,
) -> Result<Vec<u8>, ParsedCopy> {
    let fatal = |m: &str| ParsedCopy::Fatal(GptError::InvalidHeader(m.into()));
    // 16 MiB 为自定防御上限（非 UEFI 要求）：几何异常即拒绝，不按表字段做巨型分配
    let n = header.number_of_partition_entries as u64;
    let es32 = header.size_of_partition_entry;
    if n == 0 || !valid_entry_size(es32) || n * es32 as u64 > 16 * 1024 * 1024 {
        return Err(fatal("implausible GPT entry geometry"));
    }
    let es = es32 as u64;
    // 损坏表的 lba/size 字段不受信任，乘加全部 checked，防溢出回绕
    let array_off = header.partition_entry_lba.checked_mul(ss).ok_or_else(|| fatal("GPT entry array offset overflow"))?;
    let array_len = n * es;
    if array_off.checked_add(array_len).ok_or_else(|| fatal("GPT entry array range overflow"))? > src.size {
        return Err(fatal("GPT entry array out of range"));
    }
    let mut raw = vec![0u8; array_len as usize];
    src.read_at(array_off, &mut raw).map_err(|e| ParsedCopy::Fatal(e.into()))?;
    if crc32(&raw) != rd_u32(sec, 88) {
        return Err(ParsedCopy::CopyDamaged(GptError::EntryArrayCorrupt { copy }));
    }
    Ok(raw)
}

/// 每条目取前 128 字节解析（头部自述的 es 可大于 128，余下为保留区）
fn parse_entry_array(raw: &[u8], n: usize, es: usize) -> Vec<GPTPartitionEntry> {
    (0..n).map(|i| parse_entry(&raw[i * es..i * es + 128])).collect()
}

/// 解析主头 + 条目数组（全部自研，写入路径用；保证主头有效）。
/// `pmbr` 由调用方（load_gpt）判定后传入——PMBR 是独立结构，不随扇区候选变化。
/// 返回 Absent = 该扇区大小下 LBA1 无 GPT
fn parse_primary(src: &FileSource, ss: u64, pmbr: PmbrSize) -> Result<ParsedCopy, GptError> {
    let mut sec = vec![0u8; ss as usize];
    if src.size < ss * 2 {
        return Ok(ParsedCopy::Absent);
    }
    src.read_at(ss, &mut sec)?;
    let header = match probe_header(&sec) {
        HeaderProbe::Present(h) => h,
        HeaderProbe::Absent => return Ok(ParsedCopy::Absent),
        // 签名在而头不可用 = 本副本损伤（另一份可能完好），不是"此处没有 GPT"
        HeaderProbe::Damaged(detail) => {
            return Ok(ParsedCopy::CopyDamaged(GptError::HeaderCorrupt { copy: GptCopyKind::Primary, detail }))
        }
    };
    let raw = match load_entry_array(src, &sec, &header, ss, GptCopyKind::Primary) {
        Ok(r) => r,
        Err(p) => return Ok(p),
    };
    // 几何自洽性校验（validate_geometry），失败即拒绝；下游可对其结果做 LBA 算术。
    // 这类失败取决于容器与头部自述，换备份副本同样不满足，故归 Fatal
    let state = match validate_geometry(&header, src.size / ss - 1) {
        Ok(s) => s,
        Err(e) => return Ok(ParsedCopy::Fatal(e)),
    };
    let n = header.number_of_partition_entries as usize;
    let es = header.size_of_partition_entry as usize;
    let entries = parse_entry_array(&raw, n, es);
    // 条目级校验放在数组 CRC 之后：错候选扇区大小已被头 CRC 筛掉，不会误 abort
    if let Err(e) = validate_entries(&entries, &header) {
        return Ok(ParsedCopy::Fatal(e));
    }
    Ok(ParsedCopy::Usable(Box::new(RawGpt { ss, header, entries, state, pmbr })))
}

/// 解析备份 GPT（盘尾）。主头不可用（撕裂/清零/数组损伤）时的回退路径——主备互备是
/// UEFI 2.10 §5.3.2 的规范要求，此刻盘尾备份是唯一能救回分区表的数据。
/// 返回的表规范化为"主头视角"（MyLBA=1 / AltLBA=last_lba），state 置
/// NeedsRepair{PrimaryUnreadable} 以便写入路径 ensure_geometry → perform_repair 重写双头重建主头
fn parse_backup(src: &FileSource, ss: u64, pmbr: PmbrSize) -> Result<ParsedCopy, GptError> {
    if src.size < ss * 2 {
        return Ok(ParsedCopy::Absent);
    }
    let file_last_lba = src.size / ss - 1;
    let mut sec = vec![0u8; ss as usize];
    src.read_at(file_last_lba * ss, &mut sec)?;
    let mut header = match probe_header(&sec) {
        HeaderProbe::Present(h) => h,
        HeaderProbe::Absent => return Ok(ParsedCopy::Absent),
        // 与主头路径同判据：签名在而头不可用是本副本损伤，不是"盘尾没有 GPT"
        HeaderProbe::Damaged(detail) => {
            return Ok(ParsedCopy::CopyDamaged(GptError::HeaderCorrupt { copy: GptCopyKind::Backup, detail }))
        }
    };
    // 备份头自述：MyLBA = 盘尾、AltLBA = 1（否则不是本盘的备份头）
    if header.primary_lba != file_last_lba || header.backup_lba != 1 {
        return Ok(ParsedCopy::Absent);
    }
    let raw = match load_entry_array(src, &sec, &header, ss, GptCopyKind::Backup) {
        Ok(r) => r,
        Err(p) => return Ok(p),
    };
    // 转成主头视角后再做几何自洽校验（validate_geometry 按主头语义检查 MyLBA==1）
    header.primary_lba = 1;
    header.backup_lba = file_last_lba;
    if let Err(e) = validate_geometry(&header, file_last_lba) {
        return Ok(ParsedCopy::Fatal(e));
    }
    let n = header.number_of_partition_entries as usize;
    let es = header.size_of_partition_entry as usize;
    let entries = parse_entry_array(&raw, n, es);
    // 与主头路径共用同一份条目校验：只修一条路径等于留洞
    if let Err(e) = validate_entries(&entries, &header) {
        return Ok(ParsedCopy::Fatal(e));
    }
    Ok(ParsedCopy::Usable(Box::new(RawGpt {
        ss,
        header,
        entries,
        state: GptState::NeedsRepair { cause: HeaderIssue::PrimaryUnreadable },
        pmbr,
    })))
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

/// 头序列化：92 字节有效 + 补零到扇区；CRC 对自身 92 字节（CRC 字段置 0）计算
fn serialize_header(h: &RawHeader, array_crc: u32, ss: u64) -> Vec<u8> {
    let mut b = vec![0u8; ss as usize];
    b[0..8].copy_from_slice(GPT_SIGNATURE);
    b[8..12].copy_from_slice(&[0x00, 0x00, 0x01, 0x00]); // revision 1.0
    b[12..16].copy_from_slice(&92u32.to_le_bytes());
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
    let crc = crc32(&b[..92]);
    b[16..20].copy_from_slice(&crc.to_le_bytes());
    b
}

/// 条目大小合规：UEFI 2.10 规定 SizeOfPartitionEntry = 128 × 2^n（128/256/512/…），
/// 前 128 字节为标准定义字段，其余 Reserved 必须为零
fn valid_entry_size(es: u32) -> bool {
    es >= 128 && es.is_power_of_two()
}

/// 条目数组序列化（含补零到扇区边界），返回 (字节, span_sectors, 数组 CRC)。
/// es > 128 时条目尾部保留区保持零；不合规的 es 无法按规范重写，显式报错
pub fn serialize_array(entries: &[GPTPartitionEntry], n: u32, es: u32, ss: u64) -> io::Result<(Vec<u8>, u64, u32)> {
    if !valid_entry_size(es) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported GPT partition entry size {es} (must be 128 × 2^n)"),
        ));
    }
    let span = (n as u64 * es as u64).div_ceil(ss);
    let mut b = vec![0u8; (span * ss) as usize];
    for (i, e) in entries.iter().enumerate().take(n as usize) {
        let off = i * es as usize;
        b[off..off + 128].copy_from_slice(&serialize_entry(e));
    }
    let crc = crc32(&b[..(n as u64 * es as u64) as usize]);
    Ok((b, span, crc))
}

/// 条目数组占用的扇区数：条目数与单条目大小取自表自身字段（4Kn 盘 128×128B = 4 扇区）
pub fn array_span_sectors(n: u32, es: u32, ss: u64) -> u64 {
    (n as u64 * es as u64).div_ceil(ss)
}

/// 重建规范化主/备头（写入路径：无论读到的是哪份副本，输出总为规范位置）
/// - 主头：MyLBA=1, Alt=last_lba, 数组=2
/// - 备头：MyLBA=last_lba, Alt=1, 数组=backup_array_lba（= last_lba − span）
///
/// `backup_array_lba` 由调用方算好传入（commit_gpt 已用 checked_sub 校验容器装得下数组）：
/// 本函数再算一次既与 serialize_array 的同一个跨度重复，又会先于调用方的下溢保护执行——
/// debug 下 panic、release 下先回绕再被调用方拦下，同一个事实两处推导
fn canonical_headers(g: &RawGpt, last_lba: u64, backup_array_lba: u64) -> (RawHeader, RawHeader) {
    let primary = RawHeader {
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: g.header.first_usable_lba,
        last_usable_lba: g.header.last_usable_lba,
        disk_guid: g.header.disk_guid,
        partition_entry_lba: 2,
        number_of_partition_entries: g.header.number_of_partition_entries,
        size_of_partition_entry: g.header.size_of_partition_entry,
    };
    let backup = RawHeader {
        primary_lba: last_lba,
        backup_lba: 1,
        first_usable_lba: g.header.first_usable_lba,
        last_usable_lba: g.header.last_usable_lba,
        disk_guid: g.header.disk_guid,
        partition_entry_lba: backup_array_lba,
        ..primary.clone()
    };
    (primary, backup)
}

/// 崩溃安全四结构序列：备数组 → 备头 → 主数组 → 主头，每步 sync。
/// 任意落点断电至少存在一份自洽副本且不一致可经 CRC 检出。
pub fn commit_gpt(src: &mut FileSource, g: &RawGpt, last_lba: u64) -> io::Result<()> {
    let (array_bytes, span, array_crc) =
        serialize_array(&g.entries, g.header.number_of_partition_entries, g.header.size_of_partition_entry, g.ss)?;
    // 跨度只算一次（serialize_array 的返回值），下溢检查先于任何头部构造
    let backup_array_lba = last_lba
        .checked_sub(span)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "disk too small to hold GPT entry array"))?;
    let (primary, backup) = canonical_headers(g, last_lba, backup_array_lba);

    src.write_at(backup_array_lba * g.ss, &array_bytes)?;
    src.sync_all()?;

    let bh = serialize_header(&backup, array_crc, g.ss);
    src.write_at(last_lba * g.ss, &bh)?;
    src.sync_all()?;

    src.write_at(2 * g.ss, &array_bytes)?;
    src.sync_all()?;

    let ph = serialize_header(&primary, array_crc, g.ss);
    src.write_at(g.ss, &ph)?;
    src.sync_all()?;
    Ok(())
}

/// 保护 MBR（LBA0）：保留 BootCode 区 0..446，仅重写 446..512。
/// SizeInLBA = 逻辑块数 − 1（UEFI 2.10 §5.2.3；LBA 字段以 logical block 计，非恒定 512 字节），
/// 超出 32 位表示范围才用 0xFFFFFFFF。本函数同时是规范化点：任何非规范值（stale / 512 口径）
/// 经此重写即收敛
pub fn ensure_protective_mbr(src: &mut FileSource) -> io::Result<()> {
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0)?;
    if lba0[510] == 0x55 && lba0[511] == 0xAA {
        let rec = &lba0[446..462]; // 槽位 1（与下方写入位置一致）
        // 与 pmbr_shape_valid 同口径：槽位 2-4 必须全零，否则 hybrid MBR 残留
        // 会使 load_gpt 拒读 GPT、0xEE 记录被 parse_mbr 当分区输出
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
    if total_sectors == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "disk has zero sectors"));
    }
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
pub struct MbrPartition {
    pub num: u32,
    pub os_type: u8,
    pub start_lba: u32,
    pub size_lba: u32,
    pub is_container: bool,
}

pub fn parse_mbr(src: &FileSource) -> io::Result<Option<Vec<MbrPartition>>> {
    let ss = src.sector_size as usize;
    if src.size < ss as u64 {
        return Ok(None);
    }
    let mut lba0 = vec![0u8; ss];
    src.read_at(0, &mut lba0)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Ok(None);
    }
    // 本工具策略（UEFI 未规定）：保护 MBR 布局的盘交由 GPT 判定，不按 msdos 解析——
    // 避免把 0xEE 记录当成分区；GPT 头/数组损坏时报 "none"，不误报假分区
    if pmbr_shape_valid(src)? {
        return Ok(None);
    }
    let mut out = Vec::new();
    for i in 0..4u32 {
        let rec = &lba0[446 + (i as usize) * 16..446 + (i as usize) * 16 + 16];
        let os_type = rec[4];
        let start = rd_u32(rec, 8);
        let size = rd_u32(rec, 12);
        if os_type == 0 || size == 0 {
            continue;
        }
        out.push(MbrPartition {
            num: i + 1,
            os_type,
            start_lba: start,
            size_lba: size,
            is_container: matches!(os_type, 0x05 | 0x0F | 0x85),
        });
    }
    Ok(Some(out))
}

/// 修改 MBR 主分区条目大小（start 不变，纯扩缩）。LBA 单位与 parse_mbr /
/// add_mdos_entry 的现有约定一致（= 扇区大小）；CHS 字段不动（现代工具惯例，
/// 内核按 LBA 解析）。经 FileSource 写入自动进 undo journal。
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

/// 便捷读取：先要求保护 MBR 形状（否则 LBA1 的残留签名即可骗过判定），再按候选扇区大小
/// （当前盘 ss → 512 → 4096）解析主头，头 CRC 与数组 CRC 均须有效，取首个命中。
/// 候选集是兼容性探测策略，非规范要求（GPT 头不自述扇区大小，镜像与设备 ss 可能不一致）
///
/// **主备恢复策略只在本函数**（UEFI 2.10 §5.3.2 要求 primary 无效时改用 backup）：
/// - 主副本可用 → 用它
/// - 主副本的**数据损伤**（CopyDamaged：头或条目数组的字节坏）→ 继续尝试备份副本，
///   因为两份副本的头与数组是各自独立写入的
/// - 主副本结构性不可用（Fatal：几何自述不合规、越出容器、条目语义非法、读取失败）→ 直接失败，
///   因为这类失败取决于容器与头部自述，备份副本同样不满足
/// - 两份都不可用 → 最终报错（绝不把"有备份"变成静默接受损坏的主副本）
pub fn load_gpt(src: &FileSource) -> Result<Option<RawGpt>, GptError> {
    // 形状不满足保护 MBR → 不是 GPT（挡残留 GPT 头）；满足后 SizeInLBA 单独分类，
    // Stale 只标记、不否决（设备扩容后即此形态），交写入路径修复
    if !pmbr_shape_valid(src)? {
        return Ok(None);
    }
    let pmbr = pmbr_size_state(src)?;
    // 记下首个"本副本损伤"的原因：只有两份副本都给不出可用的表时才需要报它
    let mut damaged: Option<GptError> = None;
    for ss in [src.sector_size, 512, 4096] {
        match parse_primary(src, ss, pmbr)? {
            ParsedCopy::Usable(g) => return Ok(Some(*g)),
            ParsedCopy::Absent => {}
            ParsedCopy::CopyDamaged(e) => damaged = damaged.or(Some(e)),
            ParsedCopy::Fatal(e) => return Err(e),
        }
    }
    // 主头不可用（撕裂/清零/数组损伤）：回退解析盘尾备份头。此刻备份是唯一能救回分区表的
    // 数据，不回退会让工具把"主副本坏但备份完好"误判为无表，进而可能在 new 时
    // 覆盖掉这份唯一的副本。返回的 state 为 NeedsRepair，写入路径会据此重建主头
    for ss in [src.sector_size, 512, 4096] {
        match parse_backup(src, ss, pmbr)? {
            ParsedCopy::Usable(g) => return Ok(Some(*g)),
            ParsedCopy::Absent => {}
            ParsedCopy::CopyDamaged(e) => damaged = damaged.or(Some(e)),
            ParsedCopy::Fatal(e) => return Err(e),
        }
    }
    // 两份副本都没有可用的表：有损伤则报出具体原因（结构化，cmd_info 据此出措辞），
    // 不静默当"无表"——后者会让后续的 new 覆盖掉或许还能救回的数据
    match damaged {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

// ---------- 分区创建 / 删除（CLI: new / add / del） ----------

/// GUID 熵源：时间 + 路径哈希（无第三方 rand 依赖；不承诺 UUIDv4 质量）。
/// disk GUID 与分区 unique GUID 共用播种
fn derive_guid(path: &std::path::Path) -> [u8; 16] {
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

/// `new`：新建空 GPT（覆盖现有表，破坏表结构但不碰分区数据区）+ 保护 MBR。
/// 几何：128 条目 × 128B，first_usable = 数组之后，last_usable = 末端 - span - 1
/// （由 UEFI 头/数组布局推导的实现约定，非规范逐字给出的公式）。
/// 最小容量 = 2·span+4 扇区（last_lba ≥ 2·span+3）：first/last_usable 可分配的
/// 紧约束，同时保证主数组 [2, 2+span) 与备数组 [last-span, last) 不重叠。
/// 512B → 68 扇区，4Kn → 12 扇区（GNU parted 对 512B 给出同一 68 下限）。
pub fn create_gpt(src: &mut FileSource, ss: u64, disk_guid: Option<[u8; 16]>) -> io::Result<()> {
    let span = (128u64 * 128).div_ceil(ss);
    let min_sectors = 2 * span + 4;
    if src.size / ss < min_sectors {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!(
            "image too small for a GPT (needs ≥{min_sectors} sectors at {ss}-byte sector size)"
        )));
    }
    let last_lba = src.size / ss - 1;
    let header = RawHeader {
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: 2 + span,
        last_usable_lba: last_lba - span - 1,
        disk_guid: disk_guid.unwrap_or_else(|| derive_guid(&src.path)),
        partition_entry_lba: 2,
        number_of_partition_entries: 128,
        size_of_partition_entry: 128,
    };
    let g = RawGpt { ss, header, entries: vec![empty_entry(); 128], state: GptState::Valid, pmbr: PmbrSize::Normal };
    commit_gpt(src, &g, last_lba)?;
    ensure_protective_mbr(src)
}

fn empty_entry() -> GPTPartitionEntry {
    GPTPartitionEntry {
        partition_type_guid: [0; 16],
        unique_partition_guid: [0; 16],
        starting_lba: 0,
        ending_lba: 0,
        attribute_bits: 0,
        partition_name: "".into(),
    }
}

/// `add`：在最低空闲槽位追加条目。校验：范围落在 [first_usable, last_usable]、
/// 与既有分区不重叠、start ≤ end。碰撞即拒绝。
pub fn add_entry(
    src: &mut FileSource,
    start: u64,
    end: u64,
    name: &str,
    type_guid: [u8; 16],
) -> Result<u32, Fail> {
    let unique = derive_guid(&src.path);
    add_entry_at(src, start, end, name, type_guid, unique)
}

/// add 的底层：显式指定 unique guid（copy 场景沿用源分区 guid）
pub fn add_entry_at(
    src: &mut FileSource,
    start: u64,
    end: u64,
    name: &str,
    type_guid: [u8; 16],
    unique_guid: [u8; 16],
) -> Result<u32, Fail> {
    let mut g = crate::gpt_policy::ensure_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    if start < g.header.first_usable_lba || end > g.header.last_usable_lba || start > end {
        return Err(Fail::refused(format!(
            "range {start}..{end} outside usable {}..{}",
            g.header.first_usable_lba, g.header.last_usable_lba
        )));
    }
    for e in &g.entries {
        if e.ending_lba == 0 {
            continue;
        }
        if !(end < e.starting_lba || start > e.ending_lba) {
            return Err(Fail::refused("range overlaps an existing partition"));
        }
    }
    let Some(slot) = g.entries.iter().position(|e| e.ending_lba == 0) else {
        return Err(Fail::refused("partition table full"));
    };
    g.entries[slot] = GPTPartitionEntry {
        partition_type_guid: type_guid,
        unique_partition_guid: unique_guid,
        starting_lba: start,
        ending_lba: end,
        attribute_bits: 0,
        partition_name: name.into(),
    };
    let last_lba = src.size / g.ss - 1;
    commit_gpt(src, &g, last_lba)?;
    ensure_protective_mbr(src)?;
    Ok((slot + 1) as u32)
}

/// GPT 分区改名。落盘字段为 36 个 UTF-16 码元（gptman 3.1.1 PartitionName.raw_buf: [u16; 36]），
/// 超长部分由 `From<&str>` 静默截断
pub fn rename_entry(src: &mut FileSource, part: u32, name: &str) -> Result<(), Fail> {
    if part == 0 {
        return Err(Fail::refused(format!("invalid partition number {part} (1-based)")));
    }
    let mut g = crate::gpt_policy::ensure_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let e = g.entries.get_mut((part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    e.partition_name = name.into();
    let last_lba = src.size / g.ss - 1;
    Ok(commit_gpt(src, &g, last_lba)?)
}

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

pub fn set_gpt_flag(src: &mut FileSource, part: u32, flag: &str, on: bool) -> Result<(), Fail> {
    if part == 0 {
        return Err(Fail::refused(format!("invalid partition number {part} (1-based)")));
    }
    // esp/boot 切换分区类型 GUID（parted gpt.c set_flag/set_system，L1633-1647、L1462-1466），
    // 不动属性位：UEFI 属性 bit48-63 为 GUID 专属区间，bit60 是 Microsoft read-only（sfdisk man）
    let set_type = matches!(flag, "esp" | "boot");
    let bit: u64 = match flag {
        "esp" | "boot" => 0,
        "legacy" | "legacy_boot" => 1 << 2,
        "hidden" => 1 << 1,
        "required" => 1 << 0,
        other => return Err(Fail::refused(format!("unknown gpt flag {other} (esp/legacy/hidden/required)"))),
    };
    let mut g = crate::gpt_policy::ensure_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let e = g.entries.get_mut((part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    if set_type {
        e.partition_type_guid = if on { ESP_TYPE_GUID } else { LINUX_FS_TYPE_GUID };
    } else if on {
        e.attribute_bits |= bit;
    } else {
        e.attribute_bits &= !bit;
    }
    let last_lba = src.size / g.ss - 1;
    Ok(commit_gpt(src, &g, last_lba)?)
}

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
    Ok(table_label(src)? == "msdos")
}

/// 标签判定："gpt" / "msdos" / "none"。
/// MBR 解析的 io 失败也走 GptError::Io（本函数只回答标签，不区分来源）
pub fn table_label(src: &FileSource) -> Result<&'static str, GptError> {
    if load_gpt(src)?.is_some() {
        return Ok("gpt");
    }
    if parse_mbr(src)?.is_some() {
        return Ok("msdos");
    }
    Ok("none")
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

/// `del`：清零条目（只清表项，分区数据区不动）
pub fn del_entry(src: &mut FileSource, part: u32) -> Result<(), Fail> {
    if part == 0 {
        return Err(Fail::refused(format!("invalid partition number {part} (1-based)")));
    }
    let mut g = crate::gpt_policy::ensure_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let e = g.entries.get_mut((part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {part} is already empty")));
    }
    *e = empty_entry();
    let last_lba = src.size / g.ss - 1;
    commit_gpt(src, &g, last_lba)?;
    Ok(ensure_protective_mbr(src)?)
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
            identity: crate::dev::TargetIdentity::resolve(&tmp, false, size),
            file: f,
            path: tmp,
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
        }
    }

    /// GPT 测试镜像：gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
    fn src_from_gpt(tag: &str, ss: u64) -> FileSource {
        let mut src = src_from(tag, fixture_gpt(ss));
        ensure_protective_mbr(&mut src).unwrap();
        src
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
        // UEFI 2.10+：SizeOfPartitionEntry ∈ {128 × 2^n}；192/136 等非法
        assert!(valid_entry_size(128));
        assert!(valid_entry_size(256));
        assert!(valid_entry_size(1024));
        assert!(!valid_entry_size(192));
        assert!(!valid_entry_size(136));
        assert!(!valid_entry_size(64));
        // es=256 序列化：前 128 字节为条目，尾部保留区为零，CRC 覆盖整个 n×es
        let e = GPTPartitionEntry {
            partition_type_guid: ESP_TYPE_GUID,
            unique_partition_guid: [0x21; 16],
            starting_lba: 34,
            ending_lba: 100,
            attribute_bits: 0,
            partition_name: "p".into(),
        };
        let (b, span, crc) = serialize_array(&[e], 1, 256, 512).unwrap();
        assert_eq!(span, 1);
        assert_eq!(b.len(), 512);
        assert_eq!(b[128..256], [0u8; 128], "reserved tail must stay zero");
        assert_eq!(crc, crate::table::crc32(&b[..256]));
        // 192 违规拒绝
        assert!(serialize_array(&[], 1, 192, 512).is_err());
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
        let span = array_span_sectors(g.header.number_of_partition_entries, g.header.size_of_partition_entry, 4096);
        assert_eq!(span, 4);
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
    fn gpt_flag_hidden_required() {
        let mut src = src_from_gpt("gflag", 512);
        set_gpt_flag(&mut src, 1, "hidden", true).unwrap();
        let g = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].attribute_bits & (1 << 1), 1 << 1);
        set_gpt_flag(&mut src, 1, "required", true).unwrap();
        let g = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].attribute_bits & (1 << 0), 1 << 0);
        // legacy（bit2）不受影响
        set_gpt_flag(&mut src, 1, "legacy", true).unwrap();
        set_gpt_flag(&mut src, 1, "hidden", false).unwrap();
        set_gpt_flag(&mut src, 1, "required", false).unwrap();
        let g = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].attribute_bits, 1 << 2);
        assert!(set_gpt_flag(&mut src, 1, "bogus", true).is_err());
    }

    #[test]
    fn type_guid_constants_are_disk_byte_order() {
        // 内核 include/linux/efi.h EFI_GUID 宏展开的落盘字节（前 3 字段小端）
        assert_eq!(ESP_TYPE_GUID, [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
        assert_eq!(LINUX_FS_TYPE_GUID, [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
    }

    #[test]
    fn gpt_flag_esp_switches_type_guid() {
        // parted gpt.c：boot/esp 标志 = 类型 GUID ↔ PARTITION_SYSTEM_GUID，
        // off 回 Linux filesystem data；属性位不动（esp≠bit60 read-only）
        let mut src = src_from_gpt("gesp", 512);
        set_gpt_flag(&mut src, 1, "esp", true).unwrap();
        let g = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, ESP_TYPE_GUID);
        assert_eq!(g.entries[0].attribute_bits, 0);
        set_gpt_flag(&mut src, 1, "boot", false).unwrap();
        let g = load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, LINUX_FS_TYPE_GUID);
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