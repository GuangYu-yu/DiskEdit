//! MBR / 保护性 MBR 的解析、尺寸改写与 MDOS 条目操作。

use super::*;

/// 签名 `55 AA`（UEFI §5.2.1 Table 5.1：byte 510 = 0x55、byte 511 = 0xAA）；
/// 本常量是它的 little-endian u16 读法，故写作 0xAA55
pub(crate) const MBR_SIGNATURE: u16 = 0xAA55;

/// 保护 MBR 分区类型：OS Type 0xEE = GPT Protective（UEFI §5.2.2–§5.2.3 Tables 5.2–5.4）
pub(crate) const PROT_MBR_TYPE: u8 = 0xEE;

/// 扩展容器分区类型（OSIndicator）：解析、尺寸改写与类型改写三处共用同一份集合，
/// 免得"哪些码是容器"在多处各写一遍
pub(crate) const MDOS_CONTAINER_TYPES: [u8; 3] = [0x05, 0x0F, 0x85];

/// 容器末 LBA（按给定表的扇区大小计）。全仓唯一的算式落点：
/// 不允许在任何调用点重写第二遍——同一事实两个来源迟早分叉。
/// 调用点各有前置（读到扇区 / ≥68 扇区等），`size < ss` 不可达；仍用 saturating
/// 收口而不是裸 `- 1`：release 未开 overflow-checks，下溢会静默回绕成 u64::MAX
///（一个"无限大的容器"），debug 下才 panic——单点算式不该把正确性押在编译模式上
pub(crate) fn container_last_lba(src: &FileSource, ss: u64) -> u64 {
    (src.size / ss).saturating_sub(1)
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
    /// 条目起始 LBA 为 0：LBA0 是分区表自身（保护 MBR / MBR）所在扇区，从它起步的分区会
    /// 覆盖表。写入侧 `add_mdos_entry` / `resize_mdos_entry` 都以 start ≥ 1 为前提
    StartZero { num: u32 },
    /// 两条目区间重叠。右侧空闲的推导（free_right_msdos）与单条目改写（resize_mdos_entry）
    /// 都以"条目互不重叠"为前提，与 GPT 侧 `ValidatedGeometry::new` 的构造点拒绝同口径
    Overlap { num_a: u32, num_b: u32, start_a: u32, start_b: u32 },
}

impl MbrDamage {
    pub fn describe(&self) -> String {
        match self {
            MbrDamage::PastEnd { num, start, size, total_sectors } => format!(
                "MBR entry {num} extends past the end of the disk (start {start} + {size} > {total_sectors} sectors)"
            ),
            MbrDamage::StartZero { num } => format!(
                "MBR entry {num} starts at LBA 0, which holds the partition table itself"
            ),
            MbrDamage::Overlap { num_a, num_b, start_a, start_b } => format!(
                "MBR entries {num_a} and {num_b} overlap (start {start_a} / {start_b})"
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

/// LBA0 末两字节是否为 MBR 签名。判型用：签名缺席时不存在与 GPT 竞争的 msdos 表
pub(crate) fn mbr_signature_present(src: &FileSource) -> io::Result<bool> {
    if src.size < 512 {
        return Ok(false);
    }
    let mut tail = [0u8; 2];
    src.read_at(510, &mut tail)?;
    Ok(u16::from_le_bytes(tail) == MBR_SIGNATURE)
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
    // 保护布局被改坏而 LBA1 仍有 CRC 自洽的主头：盘仍是 GPT，只是 LBA0 被外部工具或手工
    // 改过。按 msdos 继续解析会把被改写的槽位当作真实分区交给 mkfs/set，而后者的写入
    // 起点就在 LBA1 附近，会直接覆盖主头。判据与 table_label 的第 3 条同源
    if gpt_header_parseable(src)? {
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
        // 起始 LBA 0 = 表自身所在扇区：partition_bytes 会为它算出偏移 0 的区间，
        // mkfs/check/set 随之从盘首擦写、毁掉 LBA0 的分区表。写入侧以 start ≥ 1 为前提，
        // 读取侧必须同判，否则这条非法条目会一路送进破坏性操作
        if start == 0 {
            damage.push(MbrDamage::StartZero { num: i + 1 });
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
            is_container: MDOS_CONTAINER_TYPES.contains(&os_type),
        });
    }
    // 条目两两重叠 = 表已损坏（与 GPT 侧 ValidatedGeometry::new 的构造点拒绝同口径）：
    // 右侧空闲的推导与单条目改写都以"条目互不重叠"为前提，损坏表上放行会把扩容砸到
    // 邻区之上。记为损伤并保留条目：info 照实观察，写路径由 parse_mbr 拒绝
    for i in 0..parts.len() {
        for j in (i + 1)..parts.len() {
            let (a, b) = (&parts[i], &parts[j]);
            let ae = a.start_lba as u64 + a.size_lba as u64 - 1;
            let be = b.start_lba as u64 + b.size_lba as u64 - 1;
            if !(ae < b.start_lba as u64 || be < a.start_lba as u64) {
                damage.push(MbrDamage::Overlap {
                    num_a: a.num,
                    num_b: b.num,
                    start_a: a.start_lba,
                    start_b: b.start_lba,
                });
            }
        }
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

/// msdos 写原语的公共入口闸：盘型是 msdos，**且**这张表可操作。
///
/// 两个问题分开回答：[`table_label`] 只说"这是哪种表"——损伤不改变盘型，观察路径靠它
/// 照实报 `mbr`；可操作性由 [`parse_mbr`] 判定。盘型判定走 raw 解析，不顺带回答能不能写，
/// 所以六个写原语都要自己问这一句，否则损伤表上的写会直接落盘
fn require_writable_mbr(src: &FileSource) -> Result<(), Fail> {
    match parse_mbr(src) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(Fail::refused("not an msdos-labelled image")),
        // InvalidData 是"条目读出来了但不可操作"（越界 / 起点 0 / 两两重叠）：请求本身
        // 仍在写盘之前，按 Refused 出口，文案沿用解析侧的损伤描述；其余才是读盘故障
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(Fail::refused(e.to_string())),
        Err(e) => Err(Fail::infra_io(e)),
    }
}

/// LBA0 写入 + 读回复核：msdos 的六个写原语共用这一处。
/// `sync_all` 只说明字节交给了设备，缓存丢写与镜像层短写要在重读时才可见；写调用
/// 已经发生，故不符或读不回都落 `Fail::failed`（盘可能已改），不得报 Infra。
/// LBA0 被丢掉而这里报成功，盘随后会被读成一张旧表，写命令全部拒
fn write_lba0_verified(src: &mut WriteSource, lba0: &[u8]) -> Result<(), Fail> {
    src.write_at(0, lba0)
        .map_err(|e| Fail::failed(format!("cannot write the MBR: {e} — the on-disk state may have changed")))?;
    src.sync_all()
        .map_err(|e| Fail::failed(format!("cannot flush the MBR: {e} — the write may not have landed")))?;
    let mut got = vec![0u8; lba0.len()];
    src.read_at(0, &mut got)
        .map_err(|e| Fail::failed(format!("MBR read-back failed: {e} — the write may not have landed")))?;
    if got != lba0 {
        return Err(Fail::failed("the MBR differs from what was written — the device or image layer dropped the write"));
    }
    Ok(())
}

/// 修改 MBR 主分区条目大小（start 不变，纯扩缩）。LBA 单位与 parse_mbr /
/// add_mdos_entry 的现有约定一致（= 扇区大小）；CHS 字段不动（现代工具惯例，
/// 内核按 LBA 解析）。经 WriteSource 写入自动进 undo journal。
///
/// 崩溃语义：普通单扇区写**不是**规范层面的原子写（untorn write 需要设备显式支持，
/// Linux 内核对普通写只作 logical block 原子的行为假设），而 MBR 无 GPT 式 CRC，
/// 掉电撕裂只能靠 0x55AA 签名粗检、字段区损坏不可检出。恢复不依赖"写是原子的"，
/// 依赖的是写前已持久化的 undo journal——发现盘异常后 `undo` 整扇区重写 LBA0 即还原。
pub fn resize_mdos_entry(src: &mut WriteSource, part: u32, new_size_lba: u32) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused(format!("invalid MBR partition number {part} (1..4)")));
    }
    require_writable_mbr(src)?;
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
    if lba0[off + 4] == 0 {
        return Err(Fail::refused(format!("MBR partition {part} is empty")));
    }
    if MDOS_CONTAINER_TYPES.contains(&lba0[off + 4]) {
        return Err(Fail::refused("extended partition container cannot be resized"));
    }
    // 隐形条目（type 字节非零但 size 为零，解析侧按空槽处理）：与 set_mdos_boot /
    // set_mdos_hidden / set_mdos_type 同判据同文案——写入等于凭空造出一条解析器
    // 看不见的分区，操作者无从核对结果
    if rd_u32(&lba0[off..off + 16], 12) == 0 {
        return Err(Fail::refused(format!(
            "partition {part} has a type byte but zero sectors — the entry is invisible to the parser; `del` clears it"
        )));
    }
    // 条目自守（与 add_mdos_entry 同严格）：start + 新长度须落在盘内。现有调用方
    // 都先查过 free，但这是 pub 写入口——越界尺寸要挡在写 LBA0 之前，不能指望
    // 每个未来调用方都记得复核
    let start = u32::from_le_bytes(lba0[off + 8..off + 12].try_into().unwrap()) as u64;
    let total_sectors = src.size / ss as u64;
    if start == 0 {
        return Err(Fail::refused(format!("MBR partition {part} has invalid start LBA 0")));
    }
    // 零长度条目在解析侧按空槽处理——写进去等于隐式删分区，缩容请求不得经此入口表达
    if new_size_lba == 0 {
        return Err(Fail::refused(format!(
            "new size 0 would erase partition {part} — use `delete` to remove the entry"
        )));
    }
    if start + new_size_lba as u64 > total_sectors {
        return Err(Fail::refused(format!(
            "new size {new_size_lba} sectors from LBA {start} exceeds the disk ({total_sectors} sectors)"
        )));
    }
    // 邻接自守（与 add_mdos_entry 同判据）：其余槽位任一与新区间重叠即拒绝。命令层的
    // free 判据以"条目互不重叠"为前提——那由 parse_mbr 的 damage 口径把关，而本入口
    // 直接读 LBA0、不经那道闸，重叠必须在此挡在写 LBA0 之前
    let new_end = start + new_size_lba as u64 - 1;
    for i in 0..4u32 {
        if i + 1 == part {
            continue;
        }
        let o = &lba0[446 + (i as usize) * 16..446 + (i as usize) * 16 + 16];
        if o[4] == 0 || rd_u32(o, 12) == 0 {
            continue;
        }
        let os = u32::from_le_bytes(o[8..12].try_into().unwrap()) as u64;
        let oe = os + rd_u32(o, 12) as u64 - 1;
        if !(new_end < os || oe < start) {
            return Err(Fail::refused(format!(
                "new range [{start}, {new_end}] overlaps partition {} [{os}, {oe}]",
                i + 1
            )));
        }
    }
    lba0[off + 12..off + 16].copy_from_slice(&new_size_lba.to_le_bytes());
    write_lba0_verified(src, &lba0)
}

/// 保护 MBR 形状（UEFI 2.10 §5.2.3）：LBA0 须是有效 MBR（签名 0xAA55），槽位 1 恰为一条
/// 0xEE 记录且 StartingLBA = 1，其余三条记录全为零。
/// 只判形状、不看 SizeInLBA（见 pmbr_size_state）：设备扩容后 SizeInLBA 会过期而形状仍成立。
/// 作为 GPT 判定的前置（内核 GPT 探测同样先要求保护 MBR 形状），用于挡住残留 GPT 头。
pub(crate) fn pmbr_shape_valid(src: &FileSource) -> io::Result<bool> {
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

/// msdos：主分区条目追加（槽位 1..=4）。类型判据与 `set_mdos_type` 同源：0x00 是
/// "未用条目"的形状（本函数用它识别空闲槽），容器类型凭空多出容器语义
pub fn add_mdos_entry(src: &mut WriteSource, start: u64, end: u64, os_type: u8) -> Result<u32, Fail> {
    if os_type == 0 {
        return Err(Fail::refused("type 0x00 is the empty-entry marker — give a partition type"));
    }
    // 0xEE 是 GPT 保护记录（UEFI 2.10 §5.2.3），不是数据分区：解析侧已把它排除出
    // 可操作条目，写侧若放行会凭空造出一条解析器与内核都不当分区的"分区"
    if os_type == PROT_MBR_TYPE {
        return Err(Fail::refused(
            "type 0xEE is the GPT protective record, not a data partition — use a real partition type",
        ));
    }
    if MDOS_CONTAINER_TYPES.contains(&os_type) {
        return Err(Fail::refused("extended-container types (0x05/0x0F/0x85) are not supported — logical partitions are out of scope"));
    }
    let ss = src.sector_size;
    require_writable_mbr(src)?;
    let total = src.size / ss;
    if total == 0 {
        return Err(Fail::infra("image has zero sectors"));
    }
    // 单扇区容器没有可用区间，单独说明以免提示里出现空区间 (1..0)
    if total == 1 {
        return Err(Fail::refused("image has a single sector — no usable range"));
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
        // 隐形条目（类型字节非零但长度为 0）：与 set_mdos_boot / set_mdos_hidden /
        // set_mdos_type / resize_mdos_entry 同判据——把它当空闲槽写进去等于凭空替换掉
        // 一条解析器看不见的条目，操作者无从核对
        if rec[4] != 0 && rd_u32(rec, 12) == 0 {
            return Err(Fail::refused(format!(
                "partition {} has a type byte but zero sectors — the entry is invisible to the parser; `del` clears it",
                i + 1
            )));
        }
        if rec[4] == 0 {
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
    write_lba0_verified(src, &lba0)?;
    Ok((i + 1) as u32)
}

/// msdos：主分区条目删除（清零对应记录）
pub fn del_mdos_entry(src: &mut WriteSource, part: u32) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    require_writable_mbr(src)?;
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Err(Fail::infra("no MBR signature on target"));
    }
    let rec = &mut lba0[446 + (part as usize - 1) * 16..446 + (part as usize - 1) * 16 + 16];
    let cur = rec[4];
    if cur == 0 {
        return Err(Fail::refused(format!("partition {part} is already empty")));
    }
    // 两类条目的类型字节承担结构语义，清零等于把它们从盘上抹掉：0xEE 是 GPT 保护记录
    // （恢复 GPT 所需），容器类型是其中逻辑分区的入口——与 add_mdos_entry / set_mdos_type
    // 同判据拒绝。解析侧已把 0xEE 排除出可操作条目，此处是写路径的同一条判据
    if cur == PROT_MBR_TYPE {
        return Err(Fail::refused(format!(
            "partition {part} is the GPT protective record (0xEE) — clearing it destroys the record used to recognise and recover the GPT"
        )));
    }
    if MDOS_CONTAINER_TYPES.contains(&cur) {
        return Err(Fail::refused(format!(
            "partition {part} is an extended container (0x{cur:02X}) — its type is structural, not a partition"
        )));
    }
    rec.fill(0);
    write_lba0_verified(src, &lba0)
}

/// msdos：boot 标志（0x80）开关
pub fn set_mdos_boot(src: &mut WriteSource, part: u32, on: bool) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    require_writable_mbr(src)?;
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Err(Fail::infra("no MBR signature on target"));
    }
    let off = 446 + (part as usize - 1) * 16;
    if lba0[off + 4] == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    if rd_u32(&lba0[off..off + 16], 12) == 0 {
        return Err(Fail::refused(format!(
            "partition {part} has a type byte but zero sectors — the entry is invisible to the parser; `del` clears it"
        )));
    }
    // MBR 规范未规定 boot 标志唯一性，"至多一个"是本工具策略：置位前清掉其他
    if on {
        for i in 0..4usize {
            lba0[446 + i * 16] = 0x00;
        }
    }
    lba0[off] = if on { 0x80 } else { 0x00 };
    write_lba0_verified(src, &lba0)
}

/// msdos hidden：OSIndicator 换成 hidden 对应码（visible ↔ visible+0x10），映射取自
/// util-linux pt-mbr-partnames.h；无对应码的类型（如 0x83 Linux）显式拒绝
pub(crate) const MDOS_HIDDEN_PAIRS: &[(u8, u8)] = &[
    (0x01, 0x11), // FAT12
    (0x04, 0x14), // FAT16 <32M
    (0x06, 0x16), // FAT16
    (0x07, 0x17), // HPFS/NTFS/exFAT
    (0x0B, 0x1B), // FAT32
    (0x0C, 0x1C), // FAT32 LBA
    (0x0E, 0x1E), // FAT16 LBA
];

pub fn set_mdos_hidden(src: &mut WriteSource, part: u32, on: bool) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    require_writable_mbr(src)?;
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Err(Fail::infra("no MBR signature on target"));
    }
    let off = 446 + (part as usize - 1) * 16;
    let cur = lba0[off + 4];
    if cur == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    if rd_u32(&lba0[off..off + 16], 12) == 0 {
        return Err(Fail::refused(format!(
            "partition {part} has a type byte but zero sectors — the entry is invisible to the parser; `del` clears it"
        )));
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
    write_lba0_verified(src, &lba0)
}

/// msdos：分区类型字节直改（OSIndicator，类型名域见 util-linux pt-mbr-partnames.h）。
/// 0x00 拒绝——那是"未用条目"的形状，移除分区应走 `del`。0xEE 是 GPT 保护记录，
/// 类型字节承担结构语义，改写后解析侧不再认得出 GPT。扩展容器类型（0x05/0x0F/0x85）
/// 双向拒绝：改成容器类型会凭空多出一条容器语义，把既有容器改走别的类型则让它管辖的
/// 逻辑分区失去入口——本工具不管理逻辑分区，两个方向都不受理
pub fn set_mdos_type(src: &mut WriteSource, part: u32, t: u8) -> Result<(), Fail> {
    if !(1..=4).contains(&part) {
        return Err(Fail::refused("msdos supports primary slots 1..=4 only"));
    }
    if t == 0 {
        return Err(Fail::refused("type 0x00 marks an empty entry — use `del` to remove the partition"));
    }
    // 与 add_mdos_entry 同一判据：0xEE 是保护记录，不是可描述的条目类型
    if t == PROT_MBR_TYPE {
        return Err(Fail::refused(
            "type 0xEE is the GPT protective record, not a data partition — use a real partition type",
        ));
    }
    if MDOS_CONTAINER_TYPES.contains(&t) {
        return Err(Fail::refused("extended-container types (0x05/0x0F/0x85) are not supported — logical partitions are out of scope"));
    }
    require_writable_mbr(src)?;
    let ss = src.sector_size;
    let mut lba0 = vec![0u8; ss as usize];
    src.read_at(0, &mut lba0).map_err(Fail::infra_io)?;
    if u16::from_le_bytes([lba0[510], lba0[511]]) != MBR_SIGNATURE {
        return Err(Fail::infra("no MBR signature on target"));
    }
    let off = 446 + (part as usize - 1) * 16;
    let cur = lba0[off + 4];
    if cur == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    if rd_u32(&lba0[off..off + 16], 12) == 0 {
        return Err(Fail::refused(format!(
            "partition {part} has a type byte but zero sectors — the entry is invisible to the parser; `del` clears it"
        )));
    }
    if MDOS_CONTAINER_TYPES.contains(&cur) {
        return Err(Fail::refused(format!("partition {part} is an extended container (0x{cur:02X}) — its type is structural, not descriptive")));
    }
    // 与 del_mdos_entry 同判据：0xEE 是保护记录，改掉它会让 pmbr_shape_valid 恒假，
    // GPT 从此被判成损坏，写路径只能 fail-closed
    if cur == PROT_MBR_TYPE {
        return Err(Fail::refused(format!(
            "partition {part} is the GPT protective record (0xEE) — its type byte is structural, not descriptive"
        )));
    }
    lba0[off + 4] = t;
    write_lba0_verified(src, &lba0)
}
