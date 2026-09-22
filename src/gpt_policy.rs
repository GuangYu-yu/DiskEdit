//! GPT 修复策略层：消费 table 解析出的事实（GptState / PmbrSize / 几何），
//! 产出可执行的 RepairAction，并用 table 的写入原语落盘。
//!
//! 分层：table.rs = 事实与解析（"是什么"）/ 本模块 = 决定做什么（"怎么办"）/
//! movepart.rs = 搬移与提交（"怎么写"）。
//! "是否需要修复、修哪一类"只在这里判一次，main 与 movepart 共用同一个动作类型。

use crate::dev::FileSource;
use crate::geometry::ValidatedGeometry;
use crate::outcome::Fail;
use crate::table::{self, GptState, PmbrSize, RawGpt};
use std::io;

/// 修复动作（互斥、穷举）。不用 bool 旗标：动作本身就是状态，
/// 需要重算的 last_usable 在决策期算好后随动作携带（plan 自持，执行期不重新推导）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairAction {
    /// 无需修复
    None,
    /// 备份头停在旧末端：重写双头并把备份头搬到设备最后 LBA
    RelocateBackup {
        new_backup_lba: u64,
        new_last_usable: u64,
    },
    /// 备份头已在末端，仅保护 MBR 的 SizeInLBA 过期
    RepairProtectiveMbr,
    /// 两者都需处理
    RelocateAndRepair {
        new_backup_lba: u64,
        new_last_usable: u64,
    },
}

impl RepairAction {
    /// 动作的人类可读描述（供 plan 输出与 apply 日志共用一处措辞）；None 动作无描述。
    /// 保护 MBR 的措辞不写成因：动作只表达"做什么"，成因（stale / 512 口径）由
    /// PmbrSize 这一事实轴承载，需要细看时用 info 读
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::RelocateBackup { new_backup_lba, new_last_usable } => Some(format!(
                "relocate backup GPT → LBA {new_backup_lba} (last_usable_lba → {new_last_usable})"
            )),
            Self::RepairProtectiveMbr => Some("rewrite protective MBR (SizeInLBA not the logical-block value for this container)".to_string()),
            Self::RelocateAndRepair { new_backup_lba, new_last_usable } => Some(format!(
                "relocate backup GPT → LBA {new_backup_lba} (last_usable_lba → {new_last_usable}) + rewrite protective MBR (SizeInLBA not the logical-block value for this container)"
            )),
        }
    }

    /// 搬迁后的 last_usable_lba（仅含搬迁的动作有值）
    pub fn new_last_usable(&self) -> Option<u64> {
        match self {
            Self::RelocateBackup { new_last_usable, .. } | Self::RelocateAndRepair { new_last_usable, .. } => {
                Some(*new_last_usable)
            }
            _ => None,
        }
    }
}

/// 修复后的 last_usable_lba = 新末端 − 数组跨度 − 1（备份数组位于备份头之前）。
/// 容器容不下最小跨度、或现有分区越出新区间 → 拒绝（不写盘，纯计算）
pub fn repaired_last_usable(g: &RawGpt, file_last_lba: u64) -> io::Result<u64> {
    // 跨度取自唯一的几何构造点，不在此另算一遍 条目数 × 条目大小 ÷ 扇区
    let geom = crate::table::EntryArrayGeometry::new(
        g.ss,
        g.header.size_of_partition_entry,
        g.header.number_of_partition_entries,
        crate::table::MAX_ARRAY_BYTES,
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let span = geom.lba_span();
    let new_last_usable = file_last_lba
        .checked_sub(span)
        .and_then(|v| v.checked_sub(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "container too small for GPT backup header — refusing"))?;
    let max_end = g.entries.iter().filter(|e| e.ending_lba != 0).map(|e| e.ending_lba).max().unwrap_or(0);
    if max_end > new_last_usable {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "partitions exceed new usable range after enlarge"));
    }
    Ok(new_last_usable)
}

/// 判定该表需要哪种修复（不写盘）。两条轴：备份头是否需要搬到新末端（GptState）、
/// 保护 MBR 是否需要规范化（PmbrSize）。只有 PMBR 值大于规范值且非已知口径
/// （Inconsistent，可能是更大盘的截断副本）才拒绝自动修复
pub fn classify_repair(g: &RawGpt, file_last_lba: u64) -> io::Result<RepairAction> {
    if g.pmbr == PmbrSize::Inconsistent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "protective MBR SizeInLBA exceeds container — refusing auto-repair (use sgdisk/parted)",
        ));
    }
    let relocates = g.state != GptState::Valid;
    // stale 与 512 口径都是"可修复"：动作相同（按本容器重写），差别只在措辞
    let pmbr_needs_repair = matches!(g.pmbr, PmbrSize::NeedsRepair { .. });
    Ok(match (relocates, pmbr_needs_repair) {
        (false, false) => RepairAction::None,
        (false, true) => RepairAction::RepairProtectiveMbr,
        (true, false) => RepairAction::RelocateBackup {
            new_backup_lba: file_last_lba,
            new_last_usable: repaired_last_usable(g, file_last_lba)?,
        },
        (true, true) => RepairAction::RelocateAndRepair {
            new_backup_lba: file_last_lba,
            new_last_usable: repaired_last_usable(g, file_last_lba)?,
        },
    })
}

/// 执行修复（写入路径唯一出口）：按动作重写双头 / 保护 MBR → 重读校验必须收敛。
/// 相位判据在函数体内显式落点——
/// - 首次目标写盘之前（重读表）：失败按 Infra 报，此时一个字节都没写，"盘可能已改变"不成立
/// - 从首次写盘起（commit_gpt / ensure_protective_mbr 及其后）：失败按 Failed 报，
///   写入量已无法断定
///
/// `None` 不写盘（调用方已在决策期确认无需修复）
pub(crate) fn apply_repair(src: &mut FileSource, action: &RepairAction) -> Result<(), Fail> {
    match action {
        RepairAction::None => return Ok(()),
        // RepairProtectiveMbr：无需写 GPT（由函数尾部的 ensure_protective_mbr 统一重写，即首次写盘）
        RepairAction::RepairProtectiveMbr => {}
        RepairAction::RelocateBackup { new_backup_lba, new_last_usable }
        | RepairAction::RelocateAndRepair { new_backup_lba, new_last_usable } => {
            let mut g = table::load_gpt(src)
                .map_err(|e| Fail::infra_io(table::into_io_error(e)))?
                .ok_or_else(|| Fail::infra("GPT vanished before repair"))?;
            g.header.last_usable_lba = *new_last_usable;
            table::commit_gpt(src, &g, *new_backup_lba)?;
        }
    }
    table::ensure_protective_mbr(src)?;
    let g2 = table::load_gpt(src).map_err(|e| Fail::failed(table::into_io_error(e).to_string()))?.ok_or_else(|| Fail::failed("re-read after repair failed"))?;
    if g2.state != GptState::Valid || g2.pmbr != PmbrSize::Normal {
        return Err(Fail::failed("repair did not converge — refusing"));
    }
    Ok(())
}

/// 所有需要做空间算术的命令（add/new/del/rename/flag/resize-part/copy）都先经过这里：
/// 读取 → 判定（classify_repair）→ 产出**可操作几何**（[`ValidatedGeometry`]，其
/// `last_usable_lba` 已是修复后将生效的值）。
///
/// 构造 [`ValidatedGeometry`] 即校验，因此写入路径拿到它之后不必再问"这个几何可用吗"：
/// - 条目数组几何自洽（条目数/单条目大小/自定字节上限，见 `EntryArrayGeometry::new`）
/// - **已定义条目互不重叠**（UEFI 2.10 §5.3.1 GPT overview：Each defined partition must not
///   overlap with any other defined partition）。本工具的空间派生事实（右侧空闲、搬移打包、
///   扩容终点）全部以不重叠为前提，故它必须在构造点被验过——这是拒绝，不是能力缺陷：
///   `info` 仍读 RawGpt，能告诉用户哪两条重叠了
///
/// **绝不写盘**：入参是 `&FileSource`，类型上就没有写入能力。决策与副作用分开是各自的
/// 唯一出口——修复动作由 [`apply_repair`] 执行，调用方须在**所有事前拒绝判定之后**才调用它，
/// 否则那些 `Refused`（承诺"本次未写盘"，退出码 10）就成了假话。
///
/// 表自身的几何自洽性（主头位置、可用区、数组位置、备份头是否越界）由 table 在解析层强制；
/// plan 不走这里（plan 不写盘，修复动作由 apply 执行）。
///
/// "修复后必须两份头都有效"这一条有规范依据：UEFI 2.10 §5.3.2 规定
/// "_Both the primary and backup GPTs must be valid before an attempt is made to grow the size
/// of a physical volume_"，理由同节给出——GPT 的恢复方案依赖备份头位于设备末端，容量变化后
/// 备份头必须随之搬移。本函数被所有空间算术命令共用（含不扩容的 del），对它们而言比规范更严：
/// 规范只规定了扩容场景的下限，并未禁止其他操作也要求双头有效
pub fn resolve_geometry(src: &FileSource) -> Result<Option<(ValidatedGeometry, RepairAction)>, Fail> {
    let g = match table::load_gpt(src) {
        Ok(Some(g)) => g,
        Ok(None) => return Ok(None),
        // 解析失败与"修不了"都发生在任何写入之前 → Infra（不能提示"盘可能已改变"）
        Err(e) => return Err(Fail::infra(format!("parse failed: {e}"))),
    };
    let file_last_lba = table::container_last_lba(src, g.ss);
    // 这些拒绝（PMBR SizeInLBA 越出容器、容器装不下备份头跨度、分区越出修复后的可用区）
    // 都是盘/容器自身的异常：改请求参数也无解，故归 infra
    let action = classify_repair(&g, file_last_lba).map_err(|e| Fail::infra(e.to_string()))?;
    // 修复后的可用区上界只从动作取（决策期已算好），不再由调用点各自推导一次
    let vg = ValidatedGeometry::new(&g, file_last_lba, action.new_last_usable())
        .map_err(|e| Fail::infra(e.to_string()))?;
    Ok(Some((vg, action)))
}

// ---------- 表项编排（"读 → 判 → 修复 → 提交"） ----------
//
// 这些函数编排的是策略层的三步（解析出几何 → 事前拒绝判定 → 修复 + 提交），
// 因此住在这里而不是 codec 层：table 只提供事实、编解码与写入原语，
// "什么时候允许写、写之前必须先做什么"由本层决定

use gptman::GPTPartitionEntry;

/// "查找第 N 个**已定义**分区"的唯一出口：分区号越界与空槽各自的拒绝文案只在此写一遍。
/// 接受条目切片使 ValidatedGeometry（写入路径）与 RawGpt（诊断路径）共用同一判据与同一措辞。
/// 需要可变访问的编排函数取 [`live_index`]，只读调用点取 [`live_entry_in`]
pub(crate) fn live_index(entries: &[GPTPartitionEntry], part: u32) -> Result<usize, Fail> {
    // part 是外部输入：part==0 的 checked_sub 让"0 号分区"落进下面的 not found，
    // 而不是先在 usize 上回绕成 usize::MAX
    let idx = part.checked_sub(1).map(|i| i as usize);
    match idx.and_then(|i| entries.get(i).map(|e| (i, e))) {
        Some((i, e)) if e.ending_lba != 0 => Ok(i),
        Some(_) => Err(Fail::refused(format!("partition {part} is empty"))),
        None => Err(Fail::refused(format!("partition {part} not found"))),
    }
}

pub(crate) fn live_entry_in(entries: &[GPTPartitionEntry], part: u32) -> Result<&GPTPartitionEntry, Fail> {
    live_index(entries, part).map(|i| &entries[i])
}

/// 分区在容器字节空间的区间 `(起始字节, 长度字节)` —— **唯一实现**：诊断路径
/// （`mkfs` / `set` / `check` 的 `:N` 命中核验）与 FS 操作路径（挂载点定位、离线缩容）
/// 都从这里取，两侧不各直呼一次 `table::load_gpt`。
///
/// 返回字节而非 LBA 是刻意的：LBA 的单位取决于它来自哪张表——GPT 条目以**表自身的** ss
/// 计（4Kn 镜像未加 `--sector-size` 时 `g.ss != src.sector_size`，按容器 ss 换算会整体差
/// 8 倍，于是 info/resize 认到的区间与 mkfs/check 认到的不是同一段字节），MBR 条目以容器
/// ss 计。换算在此处完成一次，下游（`fsid::identify` 按字节区间工作）不必知道单位是谁的
///
/// 返回 `Fail` 而不是 `(码, 文案)`：前缀与码必须同源——否则调用点会各自拼 "refused: "
/// 前缀，碰上 30 就自相矛盾（打出 "refused: parse failed: ..." 却退出 30）
pub(crate) fn partition_bytes(src: &FileSource, part: u32) -> Result<(u64, u64), Fail> {
    // 无表 = 请求与目标现状不匹配(10)；表在但结构非法 = 盘内容故障(30)。
    // 与 resize / info 的 parse failed / no partition table 同一判据
    match crate::table::load_gpt(src) {
        Err(e) => Err(Fail::infra(format!("parse failed: {e}"))),
        Ok(Some(g)) => {
            let e = live_entry_in(&g.entries, part)?;
            Ok((e.starting_lba * g.ss, (e.ending_lba - e.starting_lba + 1) * g.ss))
        }
        // 无 GPT → 按 MBR 解析。不这么做的话真 MBR 盘在这里被一律当成"无表"，
        // mkfs / set label|uuid 在 MBR 上完全不可用
        Ok(None) => match crate::table::parse_mbr(src).map_err(|e| Fail::infra(format!("parse failed: {e}")))? {
            None => Err(Fail::refused("no partition table on target")),
            Some(mbr) => {
                let p = mbr.iter().find(|p| p.num == part).ok_or_else(|| {
                    Fail::refused(format!("partition {part} not found (MBR covers primary slots 1..=4)"))
                })?;
                // 扩展容器是逻辑分区的壳，不是可承载文件系统的分区
                if p.is_container {
                    return Err(Fail::refused(format!(
                        "partition {part} is an extended container (logical partitions are out of scope)"
                    )));
                }
                Ok((p.start_lba as u64 * src.sector_size, p.size_lba as u64 * src.sector_size))
            }
        },
    }
}

/// 已验证几何上的 [`live_entry_in`] 惯用形态：分区号上界来自几何的条目数
pub(crate) fn live_entry(g: &ValidatedGeometry, part: u32) -> Result<&GPTPartitionEntry, Fail> {
    live_entry_in(&g.entries, part)
}

/// add：自动派生 unique guid（与镜像路径绑定，复制盘不会拿到相同的 PARTUUID）
pub fn add_entry(
    src: &mut FileSource,
    start: u64,
    end: u64,
    name: &str,
    type_guid: [u8; 16],
) -> Result<u32, Fail> {
    let unique = table::derive_guid(&src.path);
    add_entry_at(src, start, end, name, type_guid, unique)
}

/// add 的底层：显式指定 unique guid（copy 场景沿用源分区 guid）。
/// `execute_copy` 在 durable boundary 之后调用它，依赖它"写盘前拒绝 ⇒ `Refused`、
/// 写盘后 ⇒ 压成 `io::Error`"这个分界——只换模块，不动契约
pub fn add_entry_at(
    src: &mut FileSource,
    start: u64,
    end: u64,
    name: &str,
    type_guid: [u8; 16],
    unique_guid: [u8; 16],
) -> Result<u32, Fail> {
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
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
    // 拒绝判定已全部结束，首次写盘从这里开始
    apply_repair(src, &repair)?;
    g.commit(src)?;
    table::ensure_protective_mbr(src)?;
    Ok((slot + 1) as u32)
}

/// GPT 分区改名。落盘字段为 36 个 UTF-16 码元（gptman 3.1.1 PartitionName.raw_buf: [u16; 36]），
/// 超长部分由 `From<&str>` 静默截断
pub fn rename_entry(src: &mut FileSource, part: u32, name: &str) -> Result<(), Fail> {
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    // 空槽 / 越界的判据与文案取自唯一出口；改名只需下标
    let i = live_index(&g.entries, part)?;
    g.entries[i].partition_name = name.into();
    // 拒绝判定已全部结束，首次写盘从这里开始
    apply_repair(src, &repair)?;
    Ok(g.commit(src)?)
}

/// GPT 属性旗标：属性位按 UEFI 2.10 §5（bit0=Required Partition，bit1=No Block IO
/// Protocol 即 hidden，bit2=Legacy BIOS Bootable）；esp/boot 为类型 GUID 切换。
///
/// 取值域收进枚举：别名表与拒绝文案只由这里生成，调用点与 HELP 不各抄一份名单
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GptFlag {
    /// `esp` / `boot`：切到 ESP 类型 GUID
    Esp,
    /// `legacy` / `legacy_boot`：Legacy BIOS Bootable 属性位
    Legacy,
    Hidden,
    Required,
}

impl GptFlag {
    /// 接受的写法（别名并列）。拒绝文案与 HELP_SET 都必须与它一致
    pub const NAMES: &'static str = "esp/boot, legacy/legacy_boot, hidden, required";

    pub fn parse(s: &str) -> Result<Self, Fail> {
        match s {
            "esp" | "boot" => Ok(Self::Esp),
            "legacy" | "legacy_boot" => Ok(Self::Legacy),
            "hidden" => Ok(Self::Hidden),
            "required" => Ok(Self::Required),
            other => Err(Fail::refused(format!(
                "unknown gpt flag {other} (accepted: {})",
                Self::NAMES
            ))),
        }
    }

    /// 切类型 GUID 而非改属性位（parted gpt.c set_flag/set_system）
    fn switches_type_guid(self) -> bool {
        matches!(self, Self::Esp)
    }

    /// 属性位；`Esp` 恒为 0——它走类型 GUID，不占属性位（UEFI 属性 bit48-63 为
    /// GUID 专属区间，bit60 是 Microsoft read-only，sfdisk man）
    fn attribute_bit(self) -> u64 {
        match self {
            Self::Esp => 0,
            Self::Legacy => 1 << 2,
            Self::Hidden => 1 << 1,
            Self::Required => 1 << 0,
        }
    }
}

pub fn set_gpt_flag(src: &mut FileSource, part: u32, flag: GptFlag, on: bool) -> Result<(), Fail> {
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let i = live_index(&g.entries, part)?;
    let e = &mut g.entries[i];
    if flag.switches_type_guid() {
        e.partition_type_guid = if on { table::ESP_TYPE_GUID } else { table::LINUX_FS_TYPE_GUID };
    } else if on {
        e.attribute_bits |= flag.attribute_bit();
    } else {
        e.attribute_bits &= !flag.attribute_bit();
    }
    // 拒绝判定已全部结束，首次写盘从这里开始
    apply_repair(src, &repair)?;
    Ok(g.commit(src)?)
}

/// `del`：清零条目（只清表项，分区数据区不动）
pub fn del_entry(src: &mut FileSource, part: u32) -> Result<(), Fail> {
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let i = live_index(&g.entries, part)?;
    g.entries[i] = table::empty_entry();
    // 拒绝判定已全部结束，首次写盘从这里开始
    apply_repair(src, &repair)?;
    g.commit(src)?;
    Ok(table::ensure_protective_mbr(src)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{HeaderIssue, PmbrIssue, RawHeader};

    /// 带一个分区的 GPT 夹具：编排函数的端到端行为（flags / esp 切换）在真表上验证
    fn gpt_src(tag: &str) -> crate::dev::FileSource {
        let mut src = crate::support::src_from(tag, &[0u8; 8 * 1024 * 1024]);
        table::create_gpt(&mut src, 512, None).unwrap();
        add_entry(&mut src, 2048, 4095, "p", table::LINUX_FS_TYPE_GUID).unwrap();
        src
    }

    #[test]
    fn gpt_flag_hidden_required() {
        let mut src = gpt_src("gflag");
        let flag = |s: &str| GptFlag::parse(s).unwrap();
        set_gpt_flag(&mut src, 1, flag("hidden"), true).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].attribute_bits & (1 << 1), 1 << 1);
        set_gpt_flag(&mut src, 1, flag("required"), true).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].attribute_bits & (1 << 0), 1 << 0);
        // legacy（bit2）不受影响
        set_gpt_flag(&mut src, 1, flag("legacy"), true).unwrap();
        set_gpt_flag(&mut src, 1, flag("hidden"), false).unwrap();
        set_gpt_flag(&mut src, 1, flag("required"), false).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].attribute_bits, 1 << 2);
        // 别名：boot 与 legacy_boot 各自与主名落同一变体（名单只有一份，拒绝文案由它生成）
        assert_eq!(flag("boot"), GptFlag::Esp);
        assert_eq!(flag("legacy_boot"), GptFlag::Legacy);
        assert!(GptFlag::parse("bogus").is_err());
    }

    #[test]
    fn gpt_flag_esp_switches_type_guid() {
        // parted gpt.c：boot/esp 标志 = 类型 GUID ↔ PARTITION_SYSTEM_GUID，
        // off 回 Linux filesystem data；属性位不动（esp≠bit60 read-only）
        let mut src = gpt_src("gesp");
        let flag = |s: &str| GptFlag::parse(s).unwrap();
        set_gpt_flag(&mut src, 1, flag("esp"), true).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::ESP_TYPE_GUID);
        assert_eq!(g.entries[0].attribute_bits, 0);
        set_gpt_flag(&mut src, 1, flag("boot"), false).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::LINUX_FS_TYPE_GUID);
    }

    /// 分区字节区间的单位是**表自身**的 ss，不是容器 ss：4Kn 表放在 512B 口径的容器里
    /// （4Kn 镜像未加 --sector-size，或经 512e 转接写入的 4Kn 盘）时，按容器 ss 换算会整体差
    /// 8 倍，于是 info/resize 认到的文件系统与 mkfs/check 认到的区间不是同一段字节
    #[test]
    fn partition_bytes_uses_table_sector_size() {
        let data = vec![0u8; 8 * 1024 * 1024];
        let mut src = crate::support::src_from("ss4k", &data); // 容器 ss = 512
        table::create_gpt(&mut src, 4096, None).unwrap();
        add_entry_at(&mut src, 256, 511, "p1", table::LINUX_FS_TYPE_GUID, [0x22; 16]).unwrap();
        // 表以 4096B 逻辑块自述（LBA1 落在 offset 4096），load_gpt 的候选 ss 会选出 4096
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().ss, 4096);
        assert_eq!(partition_bytes(&src, 1).unwrap(), (256 * 4096, 256 * 4096));
    }

    /// 无表、空槽、越界编号各自只可能落一种出口语义：前者 10，后两者同判据同文案
    #[test]
    fn partition_bytes_error_kinds() {
        let bare = crate::support::src_from("pb_none", &vec![0u8; 1024 * 1024]);
        assert!(matches!(partition_bytes(&bare, 1), Err(Fail::Refused(_))));
        let src = gpt_src("pb_gpt");
        assert!(matches!(partition_bytes(&src, 2), Err(Fail::Refused(m)) if m.contains("empty")));
        assert!(matches!(partition_bytes(&src, 0), Err(Fail::Refused(m)) if m.contains("not found")));
        // MBR：容器分区不可当 FS 载体
        let mut src = crate::support::src_from("pb_mbr", &vec![0u8; 2 * 1024 * 1024]);
        table::create_mbr(&mut src).unwrap();
        table::add_mdos_entry(&mut src, 63, 200, 0x83).unwrap();
        table::add_mdos_entry(&mut src, 201, 300, 0x05).unwrap();
        assert_eq!(partition_bytes(&src, 1).unwrap(), (63 * 512, (200u64 - 63 + 1) * 512));
        assert!(matches!(partition_bytes(&src, 2), Err(Fail::Refused(m)) if m.contains("container")));
    }

    /// 最小表事实：ss=512、128×128B 条目（数组跨度 = 32 扇区）、条目仅含 (start,end) 区间
    fn gpt(state: GptState, pmbr: PmbrSize, ents: &[(u64, u64)]) -> RawGpt {
        RawGpt {
            ss: 512,
            state,
            pmbr,
            header: RawHeader {
                primary_lba: 1,
                backup_lba: 999, // 旧末端：事实以 state 表达，字段值不参与判定
                first_usable_lba: 34,
                last_usable_lba: 999,
                disk_guid: [0; 16],
                partition_entry_lba: 2,
                number_of_partition_entries: 128,
                size_of_partition_entry: 128,
                header_size: 92,
            },
            entries: ents.iter().map(|&(s, e)| gptman::GPTPartitionEntry {
                partition_type_guid: [1; 16],
                unique_partition_guid: [2; 16],
                starting_lba: s,
                ending_lba: e,
                attribute_bits: 0,
                partition_name: "".into(),
            }).collect(),
        }
    }

    /// 判定矩阵：两条轴（state 是否需搬迁 / pmbr 是否需规范化）四组合 + Inconsistent 一票否决
    #[test]
    fn classify_repair_decision_matrix() {
        let valid = GptState::Valid;
        let stale = GptState::NeedsRepair { cause: HeaderIssue::BackupLbaStale { expected: 999, actual: 500 } };
        let pmbr_ok = PmbrSize::Normal;
        let pmbr_stale = PmbrSize::NeedsRepair { cause: PmbrIssue::Stale };

        assert_eq!(classify_repair(&gpt(valid, pmbr_ok, &[]), 2000).unwrap(), RepairAction::None);
        assert_eq!(
            classify_repair(&gpt(valid, pmbr_stale, &[]), 2000).unwrap(),
            RepairAction::RepairProtectiveMbr
        );
        assert_eq!(
            classify_repair(&gpt(stale, pmbr_ok, &[]), 2000).unwrap(),
            RepairAction::RelocateBackup { new_backup_lba: 2000, new_last_usable: 2000 - 32 - 1 }
        );
        assert_eq!(
            classify_repair(&gpt(stale, pmbr_stale, &[]), 2000).unwrap(),
            RepairAction::RelocateAndRepair { new_backup_lba: 2000, new_last_usable: 2000 - 32 - 1 }
        );
        // Inconsistent：可能是更大盘的截断副本，无论 state 如何都拒绝自动修复
        let inc = gpt(GptState::Valid, PmbrSize::Inconsistent, &[]);
        assert!(classify_repair(&inc, 2000).is_err());
    }

    /// 搬迁后的可用区上界：容量不足与既有分区越界都必须拒绝，绝不静默截断分区
    #[test]
    fn repaired_last_usable_bounds() {
        // 正常：new_last_usable = file_last − 跨度 − 1
        let g = gpt(GptState::Valid, PmbrSize::Normal, &[(100, 500)]);
        assert_eq!(repaired_last_usable(&g, 2000).unwrap(), 1967);
        // 分区末端越出搬迁后的可用区 → 拒绝
        assert!(repaired_last_usable(&g, 500).is_err());
        // 容器容不下 备份数组+备份头 的最小跨度 → 拒绝（下溢防护）
        assert!(repaired_last_usable(&g, 32).is_err());
        assert!(repaired_last_usable(&g, 33).is_err());
    }
}