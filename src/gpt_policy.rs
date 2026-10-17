//! GPT 修复策略层：消费 table 解析出的事实（GptState / PmbrSize / 几何），
//! 产出可执行的 RepairAction，并用 table 的写入原语落盘。
//!
//! 分层：table.rs = 事实与解析（"是什么"）/ 本模块 = 决定做什么（"怎么办"）/
//! movepart.rs = 搬移与提交（"怎么写"）。
//! "是否需要修复、修哪一类"只在这里判一次，main 与 movepart 共用同一个动作类型。

use crate::dev::{FileSource, PartSelector};
use crate::geometry::ValidatedGeometry;
use crate::outcome::Fail;
use crate::table::{self, GptState, MbrPartition, PmbrSize, RawGpt};
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

/// 条目里的最大 ending_lba（0 视为空条目）。两处作用于不同时刻的 `g`
/// （判定期 vs 锁下重读），不可跨调用缓存
fn last_used_end(g: &RawGpt) -> u64 {
    g.entries.iter().filter(|e| e.ending_lba != 0).map(|e| e.ending_lba).max().unwrap_or(0)
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
    )?;
    let span = geom.lba_span();
    let new_last_usable = file_last_lba
        .checked_sub(span)
        .and_then(|v| v.checked_sub(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "container too small for GPT backup header — refusing"))?;
    let max_end = last_used_end(g);
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
        RepairAction::RelocateBackup { .. } | RepairAction::RelocateAndRepair { .. } => {
            // 判定期的 new_backup_lba / new_last_usable 是旧快照：两步之间表或容器可能被
            // 外部改动。落盘的头必须满足自身不变量，故修复参数一律从重读结果重新推导，
            // 并经 ValidatedGeometry::new 复核（条目数、覆盖关系、备份元数据上界）后提交
            let g = table::load_gpt(src)
                .map_err(|e| Fail::infra_io(table::into_io_error(e)))?
                .ok_or_else(|| Fail::infra("GPT vanished before repair"))?;
            let file_last_lba = table::container_last_lba(src, g.ss);
            let new_last_usable = repaired_last_usable(&g, file_last_lba)
                .map_err(|e| Fail::infra(e.to_string()))?;
            let vg = ValidatedGeometry::new(&g, file_last_lba, Some(new_last_usable))
                .map_err(|e| Fail::infra(e.to_string()))?;
            vg.commit(src).map_err(|e| Fail::failed(e.to_string()))?;
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

/// 取 GPT 的可操作几何（含修复分类），没有 GPT 即拒绝——名字点明它只要 GPT：MBR 没有
/// "几何 + 修复"这一层，通用表读取走 [`read_table`]。命令层各写一遍这段 match，等于把
/// "判定 + 文案"复制到每个调用点，改一处口径就要追多处；`verb` 是命令/动作名，与
/// `fsops::check_support(verb, …)` 同款——文案的主语由调用点传入，不各写一份
pub fn require_gpt_geometry(src: &FileSource, verb: &str) -> Result<(ValidatedGeometry, RepairAction), Fail> {
    resolve_geometry(src)?.ok_or_else(|| Fail::refused(format!("{verb} requires a GPT target")))
}

// ---------- 表项编排（"读 → 判 → 修复 → 提交"） ----------
//
// 这些函数编排的是策略层的三步（解析出几何 → 事前拒绝判定 → 修复 + 提交），
// 因此住在这里而不是 codec 层：table 只提供事实、编解码与写入原语，
// "什么时候允许写、写之前必须先做什么"由本层决定

use gptman::GPTPartitionEntry;

/// 目标上的分区表（已解析）。族信息必须与条目一起交出去——`:last` 的解释按族分派
/// （GPT 跳空槽、MBR 跳扩展容器），少了它，这些规则就会被迫泄漏回调用层。
/// 扇区尺寸也留在 GPT 一侧：条目以**表自身的** ss 计，可与容器 ss 不同（4Kn 镜像未加
/// `--sector-size` 时二者差 8 倍），MBR 没有自己的扇区尺寸概念
pub(crate) enum Table {
    Gpt { entries: Vec<GPTPartitionEntry>, ss: u64 },
    Mbr(Vec<MbrPartition>),
}

impl Table {
    fn entries(&self) -> TableEntries<'_> {
        match self {
            Table::Gpt { entries, .. } => TableEntries::Gpt(entries),
            Table::Mbr(parts) => TableEntries::Mbr(parts),
        }
    }
}

/// [`resolve_part_in`] 的入参：只借条目、不复制——已持有表的调用点（写路径的几何、
/// MBR resize 已解析的条目）直接借它，不重复读盘
#[derive(Clone, Copy)]
pub(crate) enum TableEntries<'a> {
    Gpt(&'a [GPTPartitionEntry]),
    Mbr(&'a [MbrPartition]),
}

/// "读出表里的条目、没有表就拒绝"的**唯一入口**：GPT 先、MBR 后，无表（10）与结构非法（30）
/// 的出口与措辞只写在这里，[`partition_bytes`] 与 [`resolve_part`] 都从这里取，不各判一遍。
/// 盘型分派（含"GPT 布局受损"这一态）另有 [`table::table_label`]——它回答的是"该走哪条
/// 命令分支"，不给出条目
pub(crate) fn read_table(src: &FileSource) -> Result<Table, Fail> {
    match table::load_gpt(src) {
        Err(e) => Err(Fail::infra(format!("parse failed: {e}"))),
        Ok(Some(g)) => Ok(Table::Gpt { entries: g.entries, ss: g.ss }),
        // 无 GPT → 按 MBR 解析。不这么做的话真 MBR 盘在这里被一律当成"无表"，
        // mkfs / set label|uuid 在 MBR 上完全不可用
        Ok(None) => match table::parse_mbr(src) {
            Err(e) => Err(Fail::infra(format!("parse failed: {e}"))),
            Ok(Some(parts)) => Ok(Table::Mbr(parts)),
            Ok(None) => Err(Fail::refused("no partition table on target")),
        },
    }
}

/// 取号：末端扇区最大的条目。"末端互异"依赖**条目不重叠**，而该保证只在写路径成立
/// （GPT 经 [`ValidatedGeometry::new`]、MBR 经 `parse_mbr` 的 damage 口径拒绝重叠）；
/// 重叠表的读路径不过那道闸（info 对重叠表仍照常取号），末端相同的极端现场里取谁
/// 由迭代顺序决定——那已是受损表，点名冲突条目才是 info 的正事
fn last_used(used: impl Iterator<Item = (u32, u64)>) -> Option<u32> {
    used.max_by_key(|&(_, last_lba)| last_lba).map(|(num, _)| num)
}

/// GPT 侧取号：条目号 = 下标 + 1，"已用"的判据与 [`live_index`] 一致（空槽不计）。
/// 语义与分派见 [`resolve_part_in`]
pub(crate) fn last_used_gpt(entries: &[GPTPartitionEntry]) -> Option<u32> {
    last_used(
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.ending_lba != 0)
            .map(|(i, e)| ((i + 1) as u32, e.ending_lba)),
    )
}

/// MBR 侧取号：只取主分区，且**跳过扩展容器**——它是 EBR / 逻辑分区的壳，不是可操作
/// 分区（[`partition_bytes`] 对它的拒绝同口径）。空槽位不进 `parts`：`parse_mbr` 只收
/// os_type 与 size 都非 0 的记录。语义与分派见 [`resolve_part_in`]
pub(crate) fn last_used_mbr(parts: &[MbrPartition]) -> Option<u32> {
    last_used(
        parts
            .iter()
            .filter(|p| !p.is_container)
            .map(|p| (p.num, p.start_lba as u64 + p.size_lba as u64 - 1)),
    )
}

/// `:last` 的语义**只在这里**，且是纯判定、不做 I/O：交哪份条目，就在哪份快照上解释
/// 选择器。`selector` 是数字时不查表——命中核验（空槽 / 不存在 / 容器）在 [`live_entry`]
/// 与 MBR 查表那一层，不在这里判第二遍。
///
/// 命令层的顺序固定为「确认表 / 几何 → 解释选择器 → 校验操作」：无表的目标上写 `:last` 与
/// 写 `:2` 得到同一个前置错误（命令自己的表要求），错误优先级不由选择器改写。
///
/// `:last` 是**最后一个可操作分区**，不是字面上的"末端扇区最大的表项"：指到一个命令收不了
/// 的对象，等于把 `:last` 变成一个多数用法都报错的写法。按族分派：
/// - GPT：末端最靠后的已用条目
/// - MBR：末端最靠后的主分区，**扩展容器除外**
///
/// 没有可指的对象（空表 / 只有空槽 / MBR 只有容器）即拒绝
pub(crate) fn resolve_part_in(t: TableEntries<'_>, selector: PartSelector) -> Result<u32, Fail> {
    match selector {
        PartSelector::Number(n) => Ok(n),
        PartSelector::Last => match t {
            TableEntries::Gpt(entries) => last_used_gpt(entries)
                .ok_or_else(|| Fail::refused("no partition to point :last at — the table has no defined entry")),
            TableEntries::Mbr(parts) => last_used_mbr(parts)
                .ok_or_else(|| Fail::refused("no partition to point :last at — the MBR has no usable primary entry")),
        },
    }
}

/// `:last` 的便捷入口：读表 + 判定。语义全在 [`resolve_part_in`]，这里只负责把表读出来；
/// 已经持有表的调用点直接用 `_in`，不重复读盘
pub(crate) fn resolve_part(src: &FileSource, selector: PartSelector) -> Result<u32, Fail> {
    match selector {
        PartSelector::Number(n) => Ok(n),
        PartSelector::Last => resolve_part_in(read_table(src)?.entries(), selector),
    }
}

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
    partition_bytes_in(&read_table(src)?, part, src.sector_size)
}

/// 写表前一次读表得到的现场：目标分区区间 + 其余分区区间。类型化而非元组，
/// 两个成员的含义在调用点一眼可辨
#[cfg(target_os = "linux")] // 唯一消费者是 online 的写前重验
pub(crate) struct DiskSnapshot {
    /// 目标分区的 (起始字节, 长度字节)
    pub target: (u64, u64),
    /// 其余已定义分区的区间（重叠判定用）
    pub others: Vec<(u64, u64)>,
}

/// 一次读表同时问"目标区间"与"其余分区区间"——写表前的重验用它：重叠判定的邻居集
/// 必须与目标条目来自**同一份快照**，分两次读会在两读之间再开一个窗口，而邻居集
/// 恰恰是更容易被第三方改动的那一半
#[cfg(target_os = "linux")] // 唯一消费者是 online 的写前重验
pub(crate) fn disk_snapshot(src: &FileSource, part: u32) -> Result<DiskSnapshot, Fail> {
    let t = read_table(src)?;
    let target = partition_bytes_in(&t, part, src.sector_size)?;
    let numbers: Vec<u32> = match &t {
        Table::Gpt { entries, .. } => (1..=entries.len() as u32).collect(),
        Table::Mbr(parts) => parts.iter().map(|p| p.num).collect(),
    };
    let mut others = Vec::new();
    for n in numbers {
        if n == part {
            continue;
        }
        match partition_bytes_in(&t, n, src.sector_size) {
            Ok(range) => others.push(range),
            // 空槽不承载任何字节，跳过。扩展容器本身不可承载文件系统，但它覆盖的区间
            // 里放着逻辑分区：写前重验的邻居集必须含它，否则主分区越过容器时，等内核
            // 以 -EBUSY 兜底之前，重叠的表已经写下去了
            Err(Fail::Refused(_)) => {
                let Table::Mbr(parts) = &t else { continue };
                let Some(p) = parts.iter().find(|p| p.num == n && p.is_container) else {
                    continue;
                };
                others.push((
                    crate::support::lba_bytes(p.start_lba as u64, src.sector_size),
                    crate::support::lba_bytes(p.size_lba as u64, src.sector_size),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(DiskSnapshot { target, others })
}

/// [`partition_bytes`] 的读表后形态：判定与措辞只有这一份，两个入口都从这里取
fn partition_bytes_in(t: &Table, part: u32, container_ss: u64) -> Result<(u64, u64), Fail> {
    match t {
        Table::Gpt { entries, ss } => {
            let ss = *ss;
            let e = live_entry_in(entries, part)?;
            // 重叠表按设计保持可诊断（info 读 RawGpt 点名冲突条目），但本函数的调用面
            // 全是数据写入（mkfs / set label|uuid / check 的修复写入）：对这样的表执行
            // 写命令会让 info 的"refuses write operations on this table"承诺为假
            if let Some((a, b)) = crate::geometry::find_overlap(entries) {
                return Err(Fail::refused(format!(
                    "partition entries #{a} and #{b} overlap — this tool refuses write operations \
                     on this table; remove the overlapping entry with sfdisk/parted first"
                )));
            }
            // 条目在 usable 区内本不该溢出；checked 失败即表内容异常，按 Infra 如实报，
            // 不让回绕值顺着字节偏移流进 losetup / mkswap 的参数里
            let overflow = |part: u32| {
                Fail::infra(format!("partition {part} entry arithmetic overflows (entry inconsistent with geometry)"))
            };
            let Some(len_lba) = e.ending_lba.checked_sub(e.starting_lba).and_then(|d| d.checked_add(1)) else {
                return Err(overflow(part));
            };
            let Some(off) = e.starting_lba.checked_mul(ss) else {
                return Err(overflow(part));
            };
            let Some(len) = len_lba.checked_mul(ss) else {
                return Err(overflow(part));
            };
            Ok((off, len))
        }
        Table::Mbr(parts) => {
            let p = parts.iter().find(|p| p.num == part).ok_or_else(|| {
                Fail::refused(format!("partition {part} not found (MBR covers primary slots 1..=4)"))
            })?;
            // 扩展容器是逻辑分区的壳，不是可承载文件系统的分区
            if p.is_container {
                return Err(Fail::refused(format!(
                    "partition {part} is an extended container (logical partitions are out of scope)"
                )));
            }
            // MBR 的 32 位 LBA × 扇区大小在 u64 域不可能溢出；checked 是全仓换算的统一口径
            Ok((crate::support::lba_bytes(p.start_lba as u64, container_ss),
                crate::support::lba_bytes(p.size_lba as u64, container_ss)))
        }
    }
}

/// GPT 可用区右端点（字节）= (last_usable_lba + 1) × 表自身的 ss；无 GPT（MBR / 裸盘）
/// 时 `None`。盘尾的备份 GPT 与其条目数组不在可用区内：越过它的表项会让备份头与
/// 分区内容互相覆盖。在线入口的容量判据只按裸盘长，故另有这一道
#[cfg(target_os = "linux")] // 唯一消费者是 online 的容量判据
pub(crate) fn usable_end_bytes(src: &FileSource) -> Result<Option<u64>, Fail> {
    match table::load_gpt(src) {
        Err(e) => Err(Fail::infra(format!("parse failed: {e}"))),
        Ok(None) => Ok(None),
        Ok(Some(g)) => g
            .header
            .last_usable_lba
            .checked_add(1)
            .and_then(|lba| lba.checked_mul(g.ss))
            .map(Some)
            .ok_or_else(|| Fail::infra("last_usable_lba arithmetic overflows")),
    }
}

/// 已验证几何上的 [`live_entry_in`] 惯用形态。几何的条目数组在解析时即按表声明的条目数
/// 定长（`table.rs` 按 `number_of_partition_entries` 分配），故上界就是 `entries.len()`，
/// 与 `entry_index` 的 `entry_count` 是同一个数
pub(crate) fn live_entry(g: &ValidatedGeometry, part: u32) -> Result<&GPTPartitionEntry, Fail> {
    live_entry_in(g.entries(), part)
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

/// add 的底层：显式指定 unique guid（copy 传入新生成的 v4 GUID，`--clone` 时
/// 传入源分区的 GUID）。`execute_copy` 在 durable boundary 之后调用它，依赖它
/// "写盘前拒绝 ⇒ `Refused`、写盘后 ⇒ 压成 `io::Error`"这个分界——只换模块，不动契约
pub fn add_entry_at(
    src: &mut FileSource,
    start: u64,
    end: u64,
    name: &str,
    type_guid: [u8; 16],
    unique_guid: [u8; 16],
) -> Result<u32, Fail> {
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    if start < g.first_usable_lba() || end > g.last_usable_lba() || start > end {
        return Err(Fail::refused(format!(
            "range {start}..{end} outside usable {}..{}",
            g.first_usable_lba(), g.last_usable_lba()
        )));
    }
    for e in g.entries() {
        if e.ending_lba == 0 {
            continue;
        }
        if !(end < e.starting_lba || start > e.ending_lba) {
            return Err(Fail::refused("range overlaps an existing partition"));
        }
    }
    let Some(slot) = g.entries().iter().position(|e| e.ending_lba == 0) else {
        return Err(Fail::refused("partition table full"));
    };
    g.update_entry(slot, |e| {
        *e = GPTPartitionEntry {
            partition_type_guid: type_guid,
            unique_partition_guid: unique_guid,
            starting_lba: start,
            ending_lba: end,
            attribute_bits: 0,
            partition_name: name.into(),
        };
    })
    .map_err(|e| Fail::infra(format!("geometry edit rejected: {e}")))?;
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
    let i = live_index(g.entries(), part)?;
    g.update_entry(i, |e| e.partition_name = name.into())
        .map_err(|e| Fail::infra(format!("geometry edit rejected: {e}")))?;
    // 拒绝判定已全部结束，首次写盘从这里开始
    apply_repair(src, &repair)?;
    Ok(g.commit(src)?)
}

/// GPT 分区类型 GUID 直改（UEFI 2.10 §5.3.2 PartitionTypeGUID）。全零拒绝：
/// 零 GUID 是"未用条目"的形状，写零等于把在用条目改成空壳，移除分区应走 `del`
pub fn set_type_guid(src: &mut FileSource, part: u32, guid: [u8; 16]) -> Result<(), Fail> {
    if guid == [0u8; 16] {
        return Err(Fail::refused("type GUID cannot be all-zero (that marks an unused entry — use `del`)"));
    }
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let i = live_index(g.entries(), part)?;
    g.update_entry(i, |e| e.partition_type_guid = guid)
        .map_err(|e| Fail::infra(format!("geometry edit rejected: {e}")))?;
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
    let i = live_index(g.entries(), part)?;
    // 先算好新类型与属性位（ESP off 的 FS 识别要读盘），再一次性写回：
    // 读盘失败时不留下半改的条目
    let cur = g.entries()[i].clone();
    let mut type_guid = cur.partition_type_guid;
    let mut attributes = cur.attribute_bits;
    if flag.switches_type_guid() {
        if on {
            type_guid = table::ESP_TYPE_GUID;
        } else {
            // off 只对已处 ESP 类型的条目做类型回退（parted gpt.c set_flag：先比对当前
            // 类型，非 ESP 不动 GUID）；关闭一个未生效的标志不应改写分区现有类型。
            // 回退落点按盘上实际 FS 定：FAT/NTFS → Microsoft basic data，其余（含识别
            // 不出的区域）→ Linux filesystem data——这是类型回退而非"恢复原类型"，
            // 原 GUID 未被保存，无从恢复
            if cur.partition_type_guid == table::ESP_TYPE_GUID {
                let base = cur.starting_lba.checked_mul(g.sector_size()).ok_or_else(|| {
                    Fail::infra("partition start overflows byte offset")
                })?;
                // checked 换算与 partition_bytes 同口径：条目来自盘上内容，回绕会把
                // 错误的字节区间送进 identify
                let len = cur.ending_lba.checked_sub(cur.starting_lba)
                    .and_then(|d| d.checked_add(1))
                    .and_then(|n| n.checked_mul(g.sector_size()))
                    .ok_or_else(|| Fail::infra("partition length overflows byte size"))?;
                // 写盘前的盘读：失败必须是 Infra（"确定未写盘"），不能经 `?` 落成
                // Failed 而虚构一条"盘可能已改变"的警示（首次写盘在函数尾部的 commit）
                let recognized = crate::fsid::identify(src, base, len).map_err(Fail::infra_io)?;
                type_guid = match recognized {
                    Some(crate::fsid::FsKind::Vfat) | Some(crate::fsid::FsKind::Ntfs) => {
                        table::BASIC_DATA_TYPE_GUID
                    }
                    _ => table::LINUX_FS_TYPE_GUID,
                };
            }
        }
    } else if on {
        attributes |= flag.attribute_bit();
    } else {
        attributes &= !flag.attribute_bit();
    }
    g.update_entry(i, |e| {
        e.partition_type_guid = type_guid;
        e.attribute_bits = attributes;
    })
    .map_err(|e| Fail::infra(format!("geometry edit rejected: {e}")))?;
    // 拒绝判定已全部结束，首次写盘从这里开始
    apply_repair(src, &repair)?;
    Ok(g.commit(src)?)
}

/// `del`：清零条目（只清表项，分区数据区不动）
pub fn del_entry(src: &mut FileSource, part: u32) -> Result<(), Fail> {
    let (mut g, repair) = resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let i = live_index(g.entries(), part)?;
    g.update_entry(i, |e| *e = table::empty_entry())
        .map_err(|e| Fail::infra(format!("geometry edit rejected: {e}")))?;
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
        // parted gpt.c set_flag/set_system：esp on = 类型换 ESP GUID；off 仅在当前类型
        // 仍是 ESP 时回退，落点按盘上 FS 定（本夹具无 FS 签名 → Linux filesystem data）；
        // 属性位不动（esp≠bit60 read-only）
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

    #[test]
    fn gpt_flag_esp_off_leaves_non_esp_untouched() {
        // off 只回退 ESP 类型：关闭一个未生效的标志不改写分区现有类型（parted gpt.c
        // set_flag 先比对当前类型，非 ESP 原样返回）
        let mut src = gpt_src("gespkeep");
        set_gpt_flag(&mut src, 1, GptFlag::Esp, false).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::LINUX_FS_TYPE_GUID);
        // on 恒切 ESP，off 再收回到回退落点
        set_gpt_flag(&mut src, 1, GptFlag::Esp, true).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::ESP_TYPE_GUID);
        set_gpt_flag(&mut src, 1, GptFlag::Esp, false).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::LINUX_FS_TYPE_GUID);
    }

    #[test]
    fn gpt_flag_esp_off_falls_back_by_fs_content() {
        // 回退落点按盘上实际 FS：FAT 签名（FAT12/16 @0x36）→ Microsoft basic data
        let mut img = vec![0u8; 8 * 1024 * 1024];
        let part_base = 2048u64 * 512;
        img[part_base as usize + 0x36..][..8].copy_from_slice(b"FAT16   ");
        let mut src = crate::support::src_from("gespfat", &img);
        table::create_gpt(&mut src, 512, None).unwrap();
        add_entry(&mut src, 2048, 4095, "p", table::LINUX_FS_TYPE_GUID).unwrap();
        set_gpt_flag(&mut src, 1, GptFlag::Esp, true).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::ESP_TYPE_GUID);
        set_gpt_flag(&mut src, 1, GptFlag::Esp, false).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::BASIC_DATA_TYPE_GUID);
    }

    #[test]
    fn gpt_set_type_guid() {
        let mut src = gpt_src("gtype");
        set_type_guid(&mut src, 1, table::ESP_TYPE_GUID).unwrap();
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::ESP_TYPE_GUID);
        // 全零拒绝且不落盘：零 GUID 是未用条目的形状
        let e = set_type_guid(&mut src, 1, [0u8; 16]).unwrap_err();
        assert!(matches!(e, Fail::Refused(_)), "all-zero must be refused: {e:?}");
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g.entries[0].partition_type_guid, table::ESP_TYPE_GUID);
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

    /// add 的三个拒绝分支（出可用区 / 重叠 / 表满）：拒绝发生在任何写盘之前。
    /// 紧邻区间必须放行，证明重叠判据没有过宽
    #[test]
    fn add_entry_at_rejects_out_of_range_overlap_and_full_table() {
        let mut src = gpt_src("addrej");
        // 低于 first_usable_lba（夹具 512B：first = 34）
        let e = add_entry_at(&mut src, 33, 100, "x", table::LINUX_FS_TYPE_GUID, [0x44; 16]).unwrap_err();
        assert!(matches!(&e, Fail::Refused(m) if m.contains("outside usable")), "{e:?}");
        // 区间倒挂走同一判据（start > end）
        let e = add_entry_at(&mut src, 5000, 4000, "x", table::LINUX_FS_TYPE_GUID, [0x44; 16]).unwrap_err();
        assert!(matches!(&e, Fail::Refused(m) if m.contains("outside usable")), "{e:?}");
        // 与既有分区（2048..4095）重叠
        let e = add_entry_at(&mut src, 3000, 5000, "x", table::LINUX_FS_TYPE_GUID, [0x44; 16]).unwrap_err();
        assert!(matches!(&e, Fail::Refused(m) if m.contains("overlaps")), "{e:?}");
        // 紧邻不重叠：必须放行（判据是 end < e.starting_lba || start > e.ending_lba）
        assert_eq!(add_entry_at(&mut src, 4096, 5000, "p2", table::LINUX_FS_TYPE_GUID, [0x45; 16]).unwrap(), 2);
        // 表满：填满剩余槽位后 add 必须拒绝——放行会静默丢掉用户刚给的数据布局
        let mut g = table::load_gpt(&src).unwrap().unwrap();
        for (i, e) in g.entries.iter_mut().enumerate().skip(2) {
            *e = gptman::GPTPartitionEntry {
                partition_type_guid: table::LINUX_FS_TYPE_GUID,
                unique_partition_guid: [0x46; 16],
                starting_lba: 6000 + i as u64,
                ending_lba: 6000 + i as u64,
                attribute_bits: 0,
                partition_name: "f".into(),
            };
        }
        let last = src.size / src.sector_size - 1;
        table::commit_gpt(&mut src, &g, last).unwrap();
        let e = add_entry_at(&mut src, 8192, 8200, "x", table::LINUX_FS_TYPE_GUID, [0x44; 16]).unwrap_err();
        assert!(matches!(&e, Fail::Refused(m) if m.contains("full")), "{e:?}");
    }

    /// 重叠条目保持可诊断（info 读 RawGpt），但 FS 写路径的字节区间出口必须拒：
    /// info 对用户承诺"this tool refuses write operations on this table"，
    /// mkfs / set label|uuid / check 都经 partition_bytes，闸就收在这
    #[test]
    fn partition_bytes_refuses_overlapping_table() {
        let mut src = gpt_src("pb_overlap");
        // 直接改条目数组绕过 add 的构造点拒绝，制造 #1（2048..4095）与 #2 重叠的表
        let mut g = table::load_gpt(&src).unwrap().unwrap();
        g.entries[1] = gptman::GPTPartitionEntry {
            partition_type_guid: table::LINUX_FS_TYPE_GUID,
            unique_partition_guid: [0x47; 16],
            starting_lba: 3000,
            ending_lba: 5000,
            attribute_bits: 0,
            partition_name: "ovl".into(),
        };
        let last = src.size / src.sector_size - 1;
        table::commit_gpt(&mut src, &g, last).unwrap();
        let e = partition_bytes(&src, 2).unwrap_err();
        assert!(matches!(&e, Fail::Refused(m) if m.contains("overlap")), "{e:?}");
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
        table::add_mdos_entry(&mut src, 201, 300, 0x83).unwrap();
        // 被测对象是"容器类型条目的区间判读"，类型字节直接写盘面：追加路径拒绝容器类型
        let mut lba0 = vec![0u8; 512];
        src.read_at(0, &mut lba0).unwrap();
        lba0[446 + 16 + 4] = 0x05;
        src.write_at(0, &lba0).unwrap();
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
                revision: 0x0001_0000,
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

    /// `:last` 的取号：末端扇区最大者。GPT 跳过空槽、MBR 跳过扩展容器——容器是 EBR /
    /// 逻辑分区的壳，不是可操作分区。没有可指的对象（空表 / 只有空槽 / 只有容器）→ None，
    /// 由 `resolve_part` 转成拒绝
    #[test]
    fn last_used_picks_the_last_operable_entry() {
        let ent = |s: u64, e: u64| gptman::GPTPartitionEntry {
            partition_type_guid: [1; 16],
            unique_partition_guid: [2; 16],
            starting_lba: s,
            ending_lba: e,
            attribute_bits: 0,
            partition_name: "".into(),
        };
        // 空槽（ending_lba == 0）不计入：取末端最大的已用条目，而不是下标最大的槽
        assert_eq!(last_used_gpt(&[ent(34, 100), ent(0, 0), ent(200, 300), ent(500, 600)]), Some(4));
        assert_eq!(last_used_gpt(&[ent(34, 100), ent(200, 300), ent(0, 0)]), Some(2));
        assert_eq!(last_used_gpt(&[ent(0, 0), ent(0, 0)]), None);
        assert_eq!(last_used_gpt(&[]), None);

        let part = |num: u32, start: u32, size: u32, is_container: bool| MbrPartition {
            num,
            os_type: if is_container { 0x05 } else { 0x83 },
            start_lba: start,
            size_lba: size,
            is_container,
        };
        // 末端 = start + size − 1：3 号起点居中，但末端最靠后的是 2 号
        assert_eq!(last_used_mbr(&[part(1, 63, 100, false), part(2, 500, 200, false), part(3, 200, 50, false)]), Some(2));
        // 容器末端最靠后也不是它：取容器之外最靠后的那条
        assert_eq!(last_used_mbr(&[part(1, 63, 100, false), part(4, 500, 5000, true)]), Some(1));
        assert_eq!(last_used_mbr(&[part(4, 500, 5000, true)]), None);
        assert_eq!(last_used_mbr(&[]), None);
    }

    /// `resolve_part_in` 是 `:last` 的唯一语义处，且是纯判定：数字原样返回（命中核验在
    /// [`live_entry`] / MBR 查表那一层），`:last` 按族分派，没有可指的对象即拒绝
    #[test]
    fn resolve_part_in_is_pure_and_typed_by_table() {
        let part = |num: u32, start: u32, size: u32, is_container: bool| MbrPartition {
            num,
            os_type: if is_container { 0x05 } else { 0x83 },
            start_lba: start,
            size_lba: size,
            is_container,
        };
        // 数字不查表：空表也照过（"分区不存在"由调用点的命中核验给出）
        assert_eq!(resolve_part_in(TableEntries::Gpt(&[]), PartSelector::Number(7)).unwrap(), 7);

        // 同一份条目、同一个选择器，解释随族变化——族信息因此不能在这一层丢掉
        let mbr = [part(1, 63, 100, false), part(4, 500, 5000, true)];
        assert_eq!(resolve_part_in(TableEntries::Mbr(&mbr), PartSelector::Last).unwrap(), 1);

        // 只有容器 / 只有空槽：没有可指的对象，拒绝
        assert!(matches!(
            resolve_part_in(TableEntries::Mbr(&mbr[1..]), PartSelector::Last),
            Err(Fail::Refused(m)) if m.contains("no usable primary entry")
        ));
        assert!(matches!(
            resolve_part_in(TableEntries::Gpt(&[]), PartSelector::Last),
            Err(Fail::Refused(m)) if m.contains("no defined entry")
        ));
    }
}