//! GPT 头与条目数组的解析、校验和状态判定（只读）。

use super::*;

/// GPT 头签名（UEFI Specification §5.3.2 Table 5.5：Header offset 0、长 8 字节 ASCII）
pub(crate) const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// 用户可见的"哪一份 GPT 副本"。用于把副本级的损伤讲清楚（主头/主数组坏 vs 备头/备数组坏），
/// 而不是笼统地说"GPT 头坏"
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GptCopyKind {
    Primary,
    Backup,
}

impl GptCopyKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Backup => "backup",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawHeader {
    /// 规范 Revision 字段（UEFI 2.10 §5.3.2 Table 5.5，字节序 0x00010000 = 1.0）。
    /// 与 HeaderSize 同口径：读到的值随头带回，重写按原值输出，不固定为 1.0
    pub revision: u32,
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
    /// 头是本工具直接映射的盘上字节，改写不能被固定为 92 而丢掉输入。
    /// 92..HeaderSize 之间是规范规定**必须为零**的保留区（同表 Table 5.5），重写时
    /// 清零是按规范 normalization，不构成丢输入——非零保留区本身即不合规盘面
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

pub(crate) fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

pub(crate) fn rd_u64(b: &[u8], off: usize) -> u64 {
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
pub(crate) enum HeaderProbe {
    /// 签名不匹配：该候选扇区大小下此处没有 GPT
    Absent,
    /// 签名在，但头部自身不可用（header_size 非法 / CRC 不符）：本副本的头字节损伤
    Damaged(&'static str),
    /// 头部可用
    Present(RawHeader),
}

/// 探测 LBA1 的原始头（前 92 字节为规范固定字段，其后至 HeaderSize 为保留区；
/// UEFI 2.10 §5.3.2 Table 5.5 布局），校验签名 + 头 CRC
pub(crate) fn probe_header(sector: &[u8]) -> HeaderProbe {
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
        revision: rd_u32(sector, 8),
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
pub(crate) fn validate_geometry(
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
    // 表级不变量：主 GPT Entry Array 固定在 LBA2 起、长度为 span，故必须在 FirstUsableLBA
    // 之前结束。此界由 UEFI 的布局要求（UEFI 2.10 §5.3.1：primary GPT Partition Entry Array
    // must be located after the primary GPT Header and end before First Usable LBA）结合本
    // 项目固定的主数组起点 LBA2 推出，不是规范逐字给出的公式。它与"当前解析的是哪一份
    // 副本"无关，故置于视角分支之前、主/备同判——备份视角若漏此界，first_usable_lba=0
    // 的备份头会被接受，其自述的可用区下界随后被写入路径当作权威（放行 starting_lba=0
    // 的条目，覆盖保护 MBR 与主头）
    if header.first_usable_lba < 2 + span {
        return Err(invalid("first_usable_lba leaves no room for the entry array — invalid GPT"));
    }
    // 表级不变量：LastUsableLBA 必须位于备份条目数组之前。备份数组固定在
    // [file_last_lba − span, file_last_lba − 1]、备份头在 file_last_lba（commit_table 的
    // 落位，UEFI 2.10 §5.3.1 要求 backup Partition Entry Array 在 Last Usable LBA 之后、
    // backup header 之前），故 last_usable_lba 一旦越过 file_last_lba − span − 1，
    // 可用区就与 backup GPT 元数据区重叠。与上面的 first_usable_lba 下界同属表级判据、
    // 主备同判：下游 add/grow 以 Valid 的 last_usable_lba 为可信上界划分区范围，
    // 放行越界值等于把新分区写进备份结构区，随后被 commit_table 无条件覆盖。
    // 减法安全：此处已过 last ≤ file_last_lba 与 first ≤ last、first ≥ 2 + span 三条，
    // 故 file_last_lba ≥ 2 + span，file_last_lba − span − 1 不下溢
    if header.last_usable_lba > file_last_lba - span - 1 {
        return Err(invalid("last_usable_lba overlaps the backup GPT entry array — invalid GPT"));
    }
    match view {
        // 主副本：数组上界不越过 FirstUsableLBA。本视角用头自述的 partition_entry_lba，
        // 对声明起点后移（> 2）的头比上面的表级界更强
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
pub(crate) fn validate_entries(entries: &[GPTPartitionEntry], header: &RawHeader) -> Result<(), GptError> {
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
pub(crate) enum ParsedCopy {
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
pub(crate) fn load_entry_array(
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
pub(crate) fn parse_entry_array(raw: &[u8], n: usize, es: usize) -> Vec<GPTPartitionEntry> {
    (0..n).map(|i| parse_entry(&raw[i * es..i * es + 128])).collect()
}

/// 解析主头 + 条目数组（全部自研，写入路径用；保证主头有效）。
/// `pmbr` 由调用方（load_gpt）判定后传入——PMBR 是独立结构，不随扇区候选变化。
/// 返回 Absent = 该扇区大小下 LBA1 无 GPT；返回本副本不可用 = 头/数组损伤、自述几何越界、
/// 或这一处读不出来。任何一种都不阻止 load_gpt 继续试别的候选与盘尾备份
pub(crate) fn parse_primary(src: &FileSource, ss: u64, pmbr: PmbrSize) -> ParsedCopy {
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
///
/// 能力边界：只认"备份头恰在容器最后一个 LBA"这一种形态（`primary_lba == file_last_lba`）。
/// 镜像扩容/截断后备份头仍留在旧末端，主头一旦损坏就没有可读副本——此时所有写命令
/// fail-closed 拒绝，但"备份可救"并不成立；扩容流程须先经 apply_repair 搬移备份头
pub(crate) fn parse_backup(src: &FileSource, ss: u64, pmbr: PmbrSize) -> ParsedCopy {
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

pub(crate) fn parse_entry(b: &[u8]) -> GPTPartitionEntry {
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

/// 分区名按 UTF-16LE 解码。`from_utf16_lossy` 会把孤立代理项替换为 U+FFFD，而
/// `commit_table` 每次重写整个条目数组——任何写表命令都会把这类名字**永久**规范化。
/// 这是已接受的边界：合法 UTF-16 名字逐字节往返，只有盘上本来就是非法 UTF-16 的
/// 名字才会被改写（UEFI 2.10 §5.3.3 规定该字段是 UTF-16 字符串）
pub(crate) fn decode_name(raw: &[u8; 72]) -> gptman::PartitionName {
    let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    let s: String = String::from_utf16_lossy(&units);
    s.trim_end_matches('\0').into()
}

/// 盘尾备份是否自洽到能独立给出整张表。**只用于判型**（主头不可读、LBA0 也无可竞争的
/// 表时），`pmbr` 只作为附带的盘型元数据传给 `parse_backup`，判型不消费它
pub(crate) fn backup_table_parseable(src: &FileSource) -> io::Result<bool> {
    let pmbr = pmbr_size_state(src)?;
    for &ss in &candidate_sector_sizes(src.sector_size) {
        if matches!(parse_backup(src, ss, pmbr), ParsedCopy::Usable(_)) {
            return Ok(true);
        }
    }
    Ok(false)
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

/// PMBR 布局不成立时判断主头是否仍可解析（签名 + HeaderCRC32）。
/// 只回答"盘型"：裸签名（`gpt_signature_present`）可能只是残留字节，不足以推翻一张
/// 形状完整的 msdos 表；一份 CRC 自洽的主头则证明盘是 GPT、LBA0 只是被改坏。
/// 不读条目数组：条目是否完好由 `load_gpt` 报
pub(crate) fn gpt_header_parseable(src: &FileSource) -> io::Result<bool> {
    for &ss in &candidate_sector_sizes(src.sector_size) {
        if src.size < ss * 2 {
            continue;
        }
        let mut sec = vec![0u8; ss as usize];
        match src.read_at(ss, &mut sec) {
            Ok(()) => {
                if matches!(probe_header(&sec), HeaderProbe::Present(_)) {
                    return Ok(true);
                }
            }
            // 与 gpt_signature_present 同口径：读不出来既不能证明有也不能证明没有
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// SizeInLBA 与当前容器的关系（唯一判定点）。规范值按设备逻辑块计（UEFI 口径）；
/// 512 口径值单独识别为"非规范但已知"，不并入 Normal——并入就等于宣称它合法。
/// 扇区数超出 32 位表示范围时按规范饱和写 0xFFFFFFFF（此时两种口径期望值相同）。
/// 读取恒取前 512 字节：MBR 记录布局在 LBA0 的前 512 字节内（UEFI 2.10 §5.2.3），
/// 与容器逻辑块大小无关，4Kn 设备上同样成立——`lba0` 的定长与 `src.sector_size`
/// 的口径差在此说明，非遗漏
pub(crate) fn pmbr_size_state(src: &FileSource) -> io::Result<PmbrSize> {
    let mut lba0 = [0u8; 512];
    src.read_at(0, &mut lba0)?;
    let size = rd_u32(&lba0, 446 + 12);
    let e = size_in_lba(src.size / src.sector_size);
    let e512 = size_in_lba(src.size / 512);
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
pub(crate) fn candidate_sector_sizes(container_ss: u64) -> Vec<u64> {
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