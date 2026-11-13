//! 搬移计划的推导：由几何、表项与续传状态算出每步动作和端点。纯判定，不写盘。
//!
//! 计划型恢复只服务 `resize --allow-move`；持久状态是恢复记录里的
//! [`crate::recovery::Relocation`]（有序计划 + 双游标 + 几何守卫），本文件负责把它
//! 还原成 [`Plan`]、以及在没有现场时按当前几何现算。

use super::*;
use crate::recovery::{RelocEntry, RelocKind, Relocation};

/// chunk 字节数：命令行 --chunk（MiB）传入；checkpoint 记录该值，
/// 续传时不一致即拒绝（chunks_done 是按 chunk 计数的，换大小会错位）
pub fn chunk_bytes(mib: u64) -> io::Result<u64> {
    if !(1..=64).contains(&mib) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--chunk out of range (1..=64 MiB)"));
    }
    Ok(mib * 1024 * 1024)
}

/// 拒绝搬移的 GPT 类型 GUID（搬移改变起始 LBA）。
/// 类型 GUID 按 UEFI 规范磁盘序比较（前三字段小端、Data4 原序），不做反向编码容错。
pub(crate) fn is_forbidden_type_guid(g: &[u8; 16]) -> bool {
    [table::LUKS_TYPE_GUID, table::LVM_TYPE_GUID].contains(g)
}

#[derive(Clone)]
pub struct PlanEntry {
    pub part_num: u32,
    pub first_lba: u64,
    pub len_lba: u64,
    /// 目标起点相对旧起点的位移（扇区）；负值表示向左移动
    pub delta_lba: i64,
    /// swap：不搬数据，落位后 mkswap 重建（UUID/PARTUUID/分区号保持）
    pub is_swap: bool,
}

impl From<&PlanEntry> for RelocEntry {
    fn from(e: &PlanEntry) -> Self {
        Self {
            partition: e.part_num,
            src_first: e.first_lba,
            len: e.len_lba,
            delta: e.delta_lba,
            is_swap: e.is_swap,
        }
    }
}

impl From<&RelocEntry> for PlanEntry {
    fn from(e: &RelocEntry) -> Self {
        Self {
            part_num: e.partition,
            first_lba: e.src_first,
            len_lba: e.len,
            delta_lba: e.delta,
            is_swap: e.is_swap,
        }
    }
}

impl From<PlanKind> for RelocKind {
    fn from(k: PlanKind) -> Self {
        match k {
            PlanKind::TailPacked => Self::TailPacked,
            PlanKind::Shift => Self::Shift,
        }
    }
}

impl From<RelocKind> for PlanKind {
    fn from(k: RelocKind) -> Self {
        match k {
            RelocKind::TailPacked => Self::TailPacked,
            RelocKind::Shift => Self::Shift,
        }
    }
}

/// swap 类型判定：只认 [`table::SWAP_TYPE_GUID`] 的规范磁盘序编码
pub(crate) fn is_swap_guid(g: &[u8; 16]) -> bool {
    *g == table::SWAP_TYPE_GUID
}

/// 内容签名对"这条分区是不是 swap"的回答：`Some(true)` 是 swap（可激活且头自洽，或本机
/// 激活不了、但元数据确实是 swap 的 32K 页——与
/// [`crate::fsops::Growable::SwapUnactivatable`] 同一口径）；`Some(false)` 认出了**别的**
/// 文件系统，是正面证据；`None` 什么都没认出来，那不是"不是 swap"，只是这一侧没有答案
pub(crate) fn content_swap_evidence(p: &crate::fsid::FsProbe) -> Option<bool> {
    if p.content == crate::fsid::ContentKind::Swap
        || matches!(p.swap, Some(h) if !h.activatable)
    {
        Some(true)
    } else if p.content == crate::fsid::ContentKind::Unknown {
        None
    } else {
        Some(false)
    }
}

/// 结论的依据：两方一致，或只有一方给出答案
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapBasis {
    Agreed,
    /// 只有分区表的类型声明（内容探测失败 / 读不出来）
    Declaration,
    /// 只有内容签名（表里没有可用的类型 GUID，MBR 是其正常情形）
    Content,
    /// 记录里的已决策事实（表已提交的现场：目的区内容不再参与判定）
    Recorded,
    /// 两方都没有答案
    Neither,
}

/// 一条分区的 swap 结论，连同它的依据
pub(crate) struct SwapVerdict {
    pub(crate) is_swap: bool,
    pub(crate) basis: SwapBasis,
}

impl SwapVerdict {
    /// 记录里的已决策事实：条目已落在记录的目的位，表提交时这一条的类型已经写成 swap
    /// （或不是）。目的区此刻装的是右邻搬走前的旧内容，对它的内容签名回答的是
    /// "这里以前是什么"，不能用来推翻已提交的判定
    pub(crate) fn recorded(is_swap: bool) -> Self {
        Self { is_swap, basis: SwapBasis::Recorded }
    }

    /// 只有一方给出答案时，把"用的是哪一方"变成一句可打印的说明；两方一致时没有可说的
    pub(crate) fn note(&self, part: u32) -> Option<String> {
        let which = match self.basis {
            SwapBasis::Agreed | SwapBasis::Recorded => return None,
            SwapBasis::Declaration => {
                "the partition table's type GUID (its content signature could not be read)"
            }
            SwapBasis::Content => {
                "the content signature (the table carries no type GUID for this entry)"
            }
            SwapBasis::Neither => {
                return Some(format!(
                    "partition #{part}: neither source answers whether this is a swap area \
                     (no type GUID and no readable content signature) — treating it as non-swap \
                     and relocating the data as it is"
                ));
            }
        };
        Some(format!(
            "partition #{part}: the swap decision comes from {which} — {}",
            if self.is_swap { "treated as a swap area" } else { "treated as ordinary data" }
        ))
    }
}

/// "这条分区是不是 swap"的对账点：声明侧（分区表的类型 GUID）与证据侧（区间内的
/// 内容签名）各答一次。两方一致按一致结果；一方缺失用另一方；两方都有且冲突则拒绝；
/// 两方都缺失时按非 swap 处理、原样搬移。
pub(crate) fn reconcile_swap(
    part: u32,
    declared: Option<bool>,
    evidence: Option<bool>,
) -> Result<SwapVerdict, RunOutcome> {
    Ok(match (declared, evidence) {
        (Some(d), Some(e)) if d == e => SwapVerdict { is_swap: d, basis: SwapBasis::Agreed },
        (Some(d), Some(e)) => {
            let word = |v: bool| if v { "a swap area" } else { "an ordinary partition" };
            return Err(RunOutcome::refused(format!(
                "partition #{part}: the partition table declares {} but the content signature says {} — \
                 the two readings disagree, so this tool cannot tell whether rewriting the range would \
                 destroy a swap area; inspect the partition with `diskedit inspect` before moving it",
                word(d),
                word(e)
            )));
        }
        (Some(d), None) => SwapVerdict { is_swap: d, basis: SwapBasis::Declaration },
        (None, Some(e)) => SwapVerdict { is_swap: e, basis: SwapBasis::Content },
        // 两方都缺：按非 swap 处理、数据原样搬移
        (None, None) => SwapVerdict { is_swap: false, basis: SwapBasis::Neither },
    })
}

/// 一条被搬移条目的当前几何：盘上起点、长度与它在表里的 swap 声明。四种拒绝在这里
/// 给出——不在条目数组里、未定义、端点倒挂、LUKS / LVM PV 不可搬移
fn move_entry(geometry: &ValidatedGeometry, part: u32) -> Result<(u64, u64, bool), RunOutcome> {
    let index = geometry
        .entry_index(part)
        .ok_or_else(|| RunOutcome::refused(format!("partition #{part} is outside the entry array")))?;
    let entry = geometry.entries().get(index).ok_or_else(|| {
        RunOutcome::internal(format!("partition #{part} has no entry in the geometry"))
    })?;
    if entry.starting_lba == 0 && entry.ending_lba == 0 {
        return Err(RunOutcome::refused(format!("partition #{part} is not defined")));
    }
    if entry.starting_lba > entry.ending_lba {
        return Err(RunOutcome::internal(format!(
            "partition #{part} has an inverted range {}..{}",
            entry.starting_lba, entry.ending_lba
        )));
    }
    if is_forbidden_type_guid(&entry.partition_type_guid) {
        return Err(RunOutcome::refused(format!(
            "partition #{part} is a LUKS / LVM PV partition — this tool does not relocate it, only size \
             changes are allowed"
        )));
    }
    Ok((
        entry.starting_lba,
        entry.ending_lba - entry.starting_lba + 1,
        is_swap_guid(&entry.partition_type_guid),
    ))
}

/// 记录里这一条是否已经落在它的目的位、且当初规划过位移（表已提交的现场）
fn committed_entry(record: Option<&Relocation>, part: u32, at: u64) -> Option<&RelocEntry> {
    record?.entries.iter().find(|e| {
        e.partition == part && e.delta != 0 && e.src_first.checked_add_signed(e.delta) == Some(at)
    })
}

/// 装配与命令层预检共用的条目判定：几何 +「这一条是不是 swap」的结论。
///
/// 源区间已经在记录的目的位时，表已提交、作业停在后置步骤上：结论取记录里的已决策
/// 事实，见 [`SwapVerdict::recorded`]；返回的源起点也取记录里的搬移前起点——重建要从
/// 那里读旧头的 UUID / 卷标，落在目的位上的字节是右邻搬走前的旧内容。其余情形由
/// 声明侧（类型 GUID）与证据侧（源区间的内容签名）对账，见 [`reconcile_swap`]
pub(crate) fn move_entry_swap(
    src: &FileSource,
    geometry: &ValidatedGeometry,
    part: u32,
    record: Option<&Relocation>,
) -> Result<(u64, u64, SwapVerdict), RunOutcome> {
    let (src_first, sectors, declared) = move_entry(geometry, part)?;
    if let Some(e) = committed_entry(record, part, src_first) {
        // 表里的类型与记录不一致：表在作业之外被改过。按记录的判定重建会覆写一个已不
        // 属于 swap 的分区，按表走则记录里的重建步会消失。两种都只能停
        if declared != e.is_swap {
            return Err(RunOutcome::refused(format!(
                "partition #{part} sits at the recorded destination but its type on disk no longer \
                 matches the relocation record — refusing to finish the swap step there"
            )));
        }
        return Ok((e.src_first, sectors, SwapVerdict::recorded(e.is_swap)));
    }
    let ss = geometry.sector_size();
    let base = src_first
        .checked_mul(ss)
        .ok_or_else(|| RunOutcome::no_mutation(format!("partition #{part} start overflows byte offset")))?;
    let bytes = sectors
        .checked_mul(ss)
        .ok_or_else(|| RunOutcome::no_mutation(format!("partition #{part} length overflows byte size")))?;
    // 读不出来只是"这一侧没有答案"，不是错误：声明侧仍可独立作数
    let evidence = crate::fsid::probe_fs(src, base, bytes)
        .ok()
        .and_then(|p| content_swap_evidence(&p));
    let verdict = reconcile_swap(part, Some(declared), evidence)?;
    Ok((src_first, sectors, verdict))
}

/// plan 的进入语义。`grow` 与 `SIZE` 两条命令共用同一个续跑槽位，续跑时必须
/// 对上号：按自己的语义执行对方的 plan，就是把用户的请求静默放大或缩水
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PlanKind {
    /// 吃满右侧空闲的尾部打包（`resize grow --allow-move` / `plan --grow` 的预览）
    TailPacked,
    /// 精确落在请求新末端的最小位移（`resize SIZE + --allow-move`）
    Shift,
}

pub struct Plan {
    pub ss: u64,
    pub last_usable_lba: u64,
    pub grow_part: u32,
    pub moves: Vec<PlanEntry>,
    pub kind: PlanKind,
    /// 待执行的修复动作：`plan --grow` 只预览，写命令把它装配成表步骤经执行器落盘
    pub repair: RepairAction,
}

/// 一条搬移条目在给定块大小下的块布局，是块布局的唯一来源：命令期与续跑的块数上界
/// （[`verify_relocation_resume`]）都从这里取，同一个条目在两处算出的块数必须相等，
/// 否则续跑会把"跳过未拷贝的 chunk"放行成提交
fn entry_spans(
    first_lba: u64,
    len_lba: u64,
    delta_lba: i64,
    is_swap: bool,
    ss: u64,
    chunk_bytes: u64,
) -> Vec<crate::planner::chunk::ChunkSpan> {
    if is_swap || delta_lba == 0 || len_lba == 0 {
        return Vec::new();
    }
    match crate::planner::chunk::move_chunk_layout(len_lba, ss, delta_lba.unsigned_abs(), len_lba, chunk_bytes) {
        Some((chunk, _)) => crate::planner::chunk::spans(first_lba, len_lba, chunk / ss).collect(),
        None => Vec::new(),
    }
}

/// movable 集合与两条硬拒绝的唯一构造处：目标之后所有已定义条目（按起点升序）。
/// 端点倒挂（盘内容故障）与 LUKS/LVM PV（不可搬移）在此一并给出——
/// 两条规划路径共用，拒绝理由不会漂移
pub(crate) fn blockers_after(
    g: &ValidatedGeometry,
    target_end: u64,
) -> Result<Vec<(u32, &gptman::GPTPartitionEntry)>, RunOutcome> {
    let mut movable: Vec<(u32, &gptman::GPTPartitionEntry)> = g
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| e.ending_lba != 0 && e.starting_lba > target_end)
        .map(|(i, e)| ((i + 1) as u32, e))
        .collect();
    movable.sort_by_key(|(_, e)| e.starting_lba);
    for (num, e) in &movable {
        // 条目端点倒挂属盘内容故障（解析层已拦，这里是纵深防守）
        if e.ending_lba < e.starting_lba {
            return Err(RunOutcome::no_mutation(format!("partition {num} has ending LBA below starting LBA (corrupted table)")));
        }
        if is_forbidden_type_guid(&e.partition_type_guid) {
            return Err(RunOutcome::refused(format!("partition {num} is LUKS/LVM PV — relocation refused")));
        }
    }
    Ok(movable)
}

/// 几何求解的两种用户意图。落位算法只在这里分岔一次，其余（目标查找、阻挡集合、
/// 条目落位、溢出与越界校验、计划装配）由 [`solve_layout`] 一份实现
#[derive(Clone, Copy)]
pub(crate) enum Layout {
    /// 尾部打包：阻挡分区整体推到 `last_usable`，末端对齐、不留缝隙
    TailPacked,
    /// 最小位移：目标末端向右 `shift` 扇区，阻挡分区按起点升序依次落位，已在目标位的不动
    Shift(u64),
}

impl Layout {
    const fn kind(self) -> PlanKind {
        match self {
            Self::TailPacked => PlanKind::TailPacked,
            Self::Shift(_) => PlanKind::Shift,
        }
    }
}

/// 单条目落位的唯一实现：长度、位移、终点越界与 swap 标记只算一遍。
/// 端点倒挂与不可搬移类型已由 [`blockers_after`] 拒绝，这里只处理落点算术。
/// `is_swap` 由调用方经 [`move_entry_swap`] 给出（声明侧与内容签名对账后的结论）
fn relocate_entry(
    num: u32,
    e: &gptman::GPTPartitionEntry,
    new_first: u64,
    last_usable: u64,
    is_swap: bool,
) -> Result<PlanEntry, RunOutcome> {
    let len = e.ending_lba - e.starting_lba + 1;
    let delta = new_first
        .checked_sub(e.starting_lba)
        .ok_or_else(|| RunOutcome::refused("insufficient tail space for relocation"))?;
    let new_end = new_first
        .checked_add(len - 1)
        .ok_or_else(|| RunOutcome::refused("relocation target LBA overflows — refusing"))?;
    if new_end > last_usable {
        return Err(RunOutcome::refused(
            "insufficient tail space to relocate blockers by the requested size",
        ));
    }
    let delta_lba = i64::try_from(delta)
        .map_err(|_| RunOutcome::refused("the relocation shift exceeds a signed LBA offset — refusing"))?;
    Ok(PlanEntry {
        part_num: num,
        first_lba: e.starting_lba,
        len_lba: len,
        delta_lba,
        is_swap,
    })
}

/// 几何求解器：plan 生成只解析 + 验证 + 计算，不写盘。stale 表按「修复后」的几何
/// 求解，修复动作记入 `plan.repair`，由执行链落实。几何由调用方解析一次并下传，本函数
/// 不再读表、不再重新推导边界；每条目的 swap 结论经 [`move_entry_swap`] 给出，与
/// [`crate::assembly::assemble_relocation_plan`] 装配时用的是同一处判定
///
/// 两种意图共用同一份目标查找、阻挡集合、单条目落位与计划装配；策略只决定每条目的
/// 新起点。输出统一按起始 LBA 降序（最右侧先搬），使拷贝永不踩源
fn solve_layout(
    src: &FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    layout: Layout,
    record: Option<&Relocation>,
) -> Result<Plan, RunOutcome> {
    let target_end = gpt_policy::live_entry(g, grow_part)?.ending_lba;
    let movable = blockers_after(g, target_end)?;
    let last_usable = g.last_usable_lba();
    let moves: Vec<PlanEntry> = match layout {
        Layout::TailPacked => {
            let mut out = Vec::with_capacity(movable.len());
            // 从右端向左排：末端对齐 cursor（含），不留缝隙
            let mut cursor = last_usable;
            for (num, e) in movable.iter().rev() {
                let len = e.ending_lba - e.starting_lba + 1;
                let new_first = cursor
                    .checked_sub(len - 1)
                    .ok_or_else(|| RunOutcome::refused("insufficient tail space for relocation"))?;
                let is_swap = move_entry_swap(src, g, *num, record)?.2.is_swap;
                out.push(relocate_entry(*num, e, new_first, last_usable, is_swap)?);
                cursor = new_first.saturating_sub(1);
            }
            out
        }
        Layout::Shift(shift) => {
            if shift == 0 {
                return Err(RunOutcome::refused("shift must be positive"));
            }
            if movable.is_empty() {
                return Err(RunOutcome::refused(
                    "no partition to relocate (right side is free — no --allow-move needed)",
                ));
            }
            if shift < movable[0].1.starting_lba - target_end {
                return Err(RunOutcome::refused(
                    "shift does not exceed the free gap before the first blocker — relocation not needed",
                ));
            }
            let mut cursor = target_end
                .checked_add(shift)
                .and_then(|v| v.checked_add(1))
                .ok_or_else(|| RunOutcome::refused("relocation target LBA overflows — refusing"))?;
            let mut out = Vec::with_capacity(movable.len());
            for (num, e) in &movable {
                let new_first = cursor.max(e.starting_lba);
                let is_swap = move_entry_swap(src, g, *num, record)?.2.is_swap;
                out.push(relocate_entry(*num, e, new_first, last_usable, is_swap)?);
                let len = e.ending_lba - e.starting_lba + 1;
                cursor = new_first
                    .checked_add(len)
                    .ok_or_else(|| RunOutcome::refused("relocation target LBA overflows — refusing"))?;
            }
            out.reverse();
            out
        }
    };
    Ok(Plan {
        ss: g.sector_size(),
        last_usable_lba: last_usable,
        grow_part,
        moves,
        kind: layout.kind(),
        repair,
    })
}

/// 尾打包规划：生产路径只有恢复感知入口（[`make_plan_resuming`]）
pub(crate) fn plan_tail_packed(
    src: &FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    record: Option<&Relocation>,
) -> Result<Plan, RunOutcome> {
    solve_layout(src, g, repair, grow_part, Layout::TailPacked, record)
}

/// 最小位移规划：`resize SIZE + --allow-move` 的几何，落位算术见 [`solve_layout`]。
///
/// 落点是绝对量（同 parted `move START END` 的语义）：首个挡路分区新起点恰为
/// 目标原末端 + shift + 1，执行链的扩容终点（`min(new_first) − 1`）因此精确落在
/// 请求的新末端——既不多吞目标与挡路者之间的间隙，也不少给。后续挡路者取
/// 「紧随前一落点」与「当前位置」的较大者：已在目标位的挡路者得到 delta 0、执行器跳过。
/// 计划生成因此对部分位移后的盘面幂等——中断后 `abandon` 重跑生成与一次做成相同的
/// 落点，不会对已搬段二次位移。输出按起始 LBA 降序，各分区的目的区要么落在尾部
/// 空闲，要么落在其右侧已被搬空分区的旧位置，拷贝永不踩源
pub(crate) fn shift_plan_from(
    src: &FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    shift: u64,
    record: Option<&Relocation>,
) -> Result<Plan, RunOutcome> {
    solve_layout(src, g, repair, grow_part, Layout::Shift(shift), record)
}

/// 本次续跑选定的 LV 与它的 extent 大小
#[derive(Clone)]
pub struct LvmSelection {
    pub path: String,
    pub extent_bytes: u64,
    /// 选择时 LV 的当前字节数（扩 LV 的基线）
    pub size: u64,
}

/// 分区已提交、LVM 收尾尚未完成时需要的事实。续跑据此判定该不该扩、扩到哪里：
/// `lv_size_before` / `lv_target` 是同一次作业的基线，盘上没有第二份。
///
/// `lv_path` 只存在于本次进程内（记录不保存路径，续跑请求重新选定 LV）；
/// `None` 且两个数值为 0 表示这次只做 pvresize（未请求 `--grow-lv`）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LvmPostFacts {
    pub lv_path: Option<String>,
    pub lv_size_before: u64,
    pub lv_target: u64,
    /// 记录时的 VG extent 字节数：续跑核对 VG 的 extent 大小没有变，
    /// 目标字节数仍与 `lvextend -l +N` 的 N 一致
    pub extent_bytes: u64,
}

impl LvmPostFacts {
    /// 记录侧的基线（不含 LV 路径）
    pub(crate) fn to_record(&self) -> crate::recovery::LvmPost {
        crate::recovery::LvmPost {
            lv_size_before: self.lv_size_before,
            lv_target: self.lv_target,
            extent_bytes: self.extent_bytes,
        }
    }
}

/// 由"分区已提交、LVM 收尾未做"这一时点的三个量推出 LVM 基线。`defer` 表示本次作业
/// 允许把 LVM 收尾留到之后；未请求 `--grow-lv` 时 `sel` 为空，只做 pvresize
pub(crate) fn lvm_post_for(
    defer: bool,
    sel: Option<&LvmSelection>,
    old_bytes: u64,
    new_bytes: u64,
) -> Result<Option<LvmPostFacts>, RunOutcome> {
    if !defer || new_bytes <= old_bytes {
        return Ok(None);
    }
    let delta = new_bytes - old_bytes;
    let Some(s) = sel else {
        // 未请求 --grow-lv：partition 的新空间由 pvresize 吸收，没有 LV 级的量
        return Ok(Some(LvmPostFacts { lv_path: None, lv_size_before: 0, lv_target: 0, extent_bytes: 0 }));
    };
    let add = delta / s.extent_bytes;
    if add == 0 {
        return Err(RunOutcome::refused(format!(
            "added {delta} bytes is less than one extent ({} bytes) — the LV cannot take any of the added space",
            s.extent_bytes
        )));
    }
    let overflow = || RunOutcome::no_mutation("LV target size overflows");
    if s.size == 0 {
        return Err(RunOutcome::refused(format!("LV {} reports 0 bytes — nothing to extend", s.path)));
    }
    let grow = add.checked_mul(s.extent_bytes).ok_or_else(overflow)?;
    let lv_target = s.size.checked_add(grow).ok_or_else(overflow)?;
    Ok(Some(LvmPostFacts {
        lv_path: Some(s.path.clone()),
        lv_size_before: s.size,
        lv_target,
        extent_bytes: s.extent_bytes,
    }))
}

/// 尾打包 plan 的恢复感知版本：记录里有搬移状态时以它的有序计划为准（见
/// [`resume_outcome`]），否则照常现算。几何与修复动作由调用方解析（一条命令解析
/// 一次，往下传），本函数不读表
pub fn make_plan_resuming(
    src: &FileSource,
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<Plan, RunOutcome> {
    match resume_outcome(relocation, g, repair.clone(), grow_part)? {
        // 作业身份比对：本入口的进入语义是"吃满右侧"（TailPacked）。槽上那份若是精确
        // SIZE 的最小位移 plan，它的终点停在旧请求的末端——按 grow 收尾等于把本次请求
        // 静默换成旧作业的续跑。判据与 SIZE 入口共用一处（见 make_plan_shift_resuming）
        ResumeRunOutcome::Belongs(p) => match p.kind {
            PlanKind::TailPacked => Ok(p),
            PlanKind::Shift => Err(plan_job_owns_target(p.kind, false)),
        },
        // 槽位是别的分区的作业：现算的 plan 与那份作业的现场不符，打印出来会与
        // 盘上未收尾的事自相矛盾，执行器也只会得到一次注定失败的比对
        ResumeRunOutcome::Occupied => Err(foreign_checkpoint_refusal()),
        ResumeRunOutcome::Empty => plan_tail_packed(src, g, repair, grow_part, relocation),
    }
}

/// 本次 SIZE 请求相对目标当前末端的形状。续跑判定必须区分三者：缩容与"尺寸未变"
/// 都算不出正位移，而只有后者可能是原样重跑当初的扩容命令
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ShiftIntent {
    /// 扩到更大的末端：带位移量
    Grow(u64),
    /// 请求尺寸与现状相同（缩容守卫链与搬移都不需要）
    NoOp,
    /// 缩容：不搬移任何分区，新末端更靠左，右侧只会更空
    Shrink,
}

/// 中断感知的最小位移 plan：记录里有搬移状态时以它的有序计划为准（见
/// [`resume_outcome`]），否则按 `intent` 现算。`Shrink` 一律拒绝并指向续跑/放弃
/// 两条出路——记录里未收尾的 plan 属于当初那条扩容请求，执行它就是把缩容静默放大
/// 成旧扩容（违反 plan 的"执行方即请求方"归属）。`NoOp` 只在表已提交、只剩收尾时
/// 出现：只有 kind 相符且计划终点恰等于当前末端，才是同一作业的续跑。几何与修复
/// 动作由调用方解析并下传，本函数不读表
pub fn make_plan_shift_resuming(
    src: &FileSource,
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    intent: ShiftIntent,
) -> Result<Plan, RunOutcome> {
    match resume_outcome(relocation, g, repair.clone(), grow_part)? {
        ResumeRunOutcome::Belongs(p) => {
            let e = gpt_policy::live_entry(g, grow_part)?;
            match intent {
                // 缩容请求撞上任何搬移作业都不执行：那份作业的终点属于更大的请求
                ShiftIntent::Shrink => return Err(plan_job_owns_target(p.kind, false)),
                // 尺寸未变：只有"计划终点已在盘上"这一态是同一作业的续跑
                ShiftIntent::NoOp => {
                    if p.kind == PlanKind::Shift && plan_requested_end(&p, e.starting_lba) == Some(e.ending_lba) {
                        return Ok(p);
                    }
                    return Err(plan_job_owns_target(p.kind, false));
                }
                ShiftIntent::Grow(shift) => {
                    // 作业身份比对，两维都要对上：kind 不同 ⇒ 是另一条命令的作业；终点不同
                    // ⇒ 是同一条命令的另一次请求。两种都不可逆——搬移已写盘，表项终点照抄旧值
                    if p.kind != PlanKind::Shift {
                        return Err(plan_job_owns_target(p.kind, false));
                    }
                    // 本次请求的终点 = 目标当前末端 + 本次位移。终点是绝对量，与 ckpt 里
                    // 记下的那次请求的终点（plan_requested_end）同一坐标系，可直接比
                    let want_end = e
                        .ending_lba
                        .checked_add(shift)
                        .ok_or_else(|| RunOutcome::refused("requested size overflows the LBA range"))?;
                    if plan_requested_end(&p, e.starting_lba) != Some(want_end) {
                        return Err(plan_job_owns_target(p.kind, true));
                    }
                    return Ok(p);
                }
            }
        }
        // 与 make_plan_resuming 同判据：别的分区的作业占着槽位，本请求既不能把它
        // 当空槽、也不能按它续跑
        ResumeRunOutcome::Occupied => return Err(foreign_checkpoint_refusal()),
        ResumeRunOutcome::Empty => {}
    }
    let ShiftIntent::Grow(shift) = intent else {
        return Err(RunOutcome::refused(
            "shrink does not relocate partitions — this entry point only builds relocation plans for growth",
        ));
    };
    shift_plan_from(src, g, repair, grow_part, shift, relocation)
}

/// 该分区是否有未收尾的 plan 型搬移作业。几何上"右侧已空、可直接扩容"并不代表
/// 作业已完成——右侧变空本身可能正是搬了一半的结果，收尾步骤（剩余搬移、swap 重建、
/// FS 扩容）都还没做。当前调用方是 movepart 的测试
#[cfg(test)]
pub fn has_pending_relocation(
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<bool, RunOutcome> {
    Ok(matches!(resume_outcome(relocation, g, repair, grow_part)?, ResumeRunOutcome::Belongs(_)))
}

/// 槽位被别的分区的搬移作业占用时的拒绝理由。
pub(crate) fn foreign_checkpoint_refusal() -> RunOutcome {
    RunOutcome::refused(
        "an unfinished relocation job for a different partition is recorded on this target — it belongs \
         to the request that created it: finish it with that request, or release it with `diskedit abandon`",
    )
}

/// 续跑入口作业身份不匹配时的拒绝理由。
pub(crate) fn plan_job_owns_target(kind: PlanKind, different_size: bool) -> RunOutcome {
    match kind {
        PlanKind::TailPacked => RunOutcome::refused(
            "an unfinished `resize grow` job owns this target — re-run that command to continue it, or \
             release it with `diskedit abandon`",
        ),
        PlanKind::Shift if different_size => RunOutcome::refused(
            "an unfinished `resize SIZE --allow-move` job owns this target for a different size — re-run \
             that command with its original size to continue it, or `diskedit abandon` to release it",
        ),
        PlanKind::Shift => RunOutcome::refused(
            "an unfinished `resize SIZE --allow-move` job owns this target — re-run that command with the \
             original size to continue it, or `diskedit abandon` to release it",
        ),
    }
}

/// 作业身份里的"请求数值"一维：Shift plan 的扩容终点（`min(new_first) − 1`，与写盘
/// 实际落下的终点是同一实现）。TailPacked 的终点由几何决定、没有数值维度，返回 None
/// 表示"这一维不参与比对"；几何非法时同样返回 None——比对不上即拒绝，不把损坏现场
/// 当成"身份相符"放行
pub(crate) fn plan_requested_end(plan: &Plan, target_start: u64) -> Option<u64> {
    match plan.kind {
        PlanKind::TailPacked => None,
        PlanKind::Shift => grow_end_for(plan, target_start).ok(),
    }
}

/// 三态判定的内核：记录里的搬移状态 → 空 / 属于本分区 / 属于别的分区。
///
/// `Belongs` 时以记录里的有序计划为准：盘上几何已被部分执行改变，重算出的 delta 与
/// 记录不一致，会撞上执行器的恢复校验而使续传永久失败。修复动作不在记录里：执行链
/// 先绑记录、再执行修复（`cmd/resize.rs::apply_move`），中断可以落在两者
/// 之间，故续跑按本次读到的表重新分类，把动作原样带进 plan
pub(crate) fn resume_outcome(
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<ResumeRunOutcome, RunOutcome> {
    let Some(r) = relocation else {
        return Ok(ResumeRunOutcome::Empty);
    };
    if r.grow_part != grow_part {
        return Ok(ResumeRunOutcome::Occupied);
    }
    Ok(ResumeRunOutcome::Belongs(Plan {
        ss: r.ss,
        last_usable_lba: g.last_usable_lba(),
        grow_part: r.grow_part,
        moves: r.entries.iter().map(PlanEntry::from).collect(),
        kind: r.kind.into(),
        repair,
    }))
}

/// resume 判定的三态结论。
pub(crate) enum ResumeRunOutcome {
    Empty,
    /// ckpt 属于本次请求的分区：plan 自持（盘上几何已被部分执行改变，现算必错）
    Belongs(Plan),
    /// 槽位被别的分区的 relocation ckpt 占用；`has_pending_relocation` 只回答"plan 型
    /// 搬移"，搬移路径必须拒绝——现算的 plan 与那份作业的现场不符
    Occupied,
}

// ---------- 持久状态 ----------
//
// 落盘状态的判据：**只持久化盘上无法判定到可枚举有限状态的事实**。搬移的持久状态
// 全部收在恢复记录的 [`crate::recovery::Relocation`] 里：有序计划、双游标、块字节、
// 作业语义与几何守卫。"表项是否已提交"由盘上几何直接给出（commit 写绝对 LBA、重放
// 幂等），不另存；"搬到第几 chunk"从数据区只看得出"已覆盖"、看不出"到哪"，必须存

/// 扩容终点：`moves` 是按"末→首"的尾部紧凑打包序，各分区新起点中最小者即本次腾出空间的
/// 左边界，故 grow_end = min(new_first) − 1；无 movable 时扩到 last_usable_lba。
/// 加法一律 checked：越界/损坏表下报错而非回绕（回绕会把荒谬的 LBA 写进表再交给 resize）。
/// `target_start`：moves 可整份来自 ckpt，其 first+delta 的下界只到 LBA 1——终点低于
/// 目标分区自身起点就是端点倒挂的表项。上、下两个边界在此收口成唯一判据
pub fn grow_end_for(plan: &Plan, target_start: u64) -> io::Result<u64> {
    grow_end_of(
        plan.moves.iter().map(|m| (m.first_lba, m.delta_lba)),
        plan.last_usable_lba,
        target_start,
    )
}

/// 同一公式的记录侧入口：续跑校验只能读记录（[`Relocation`]），不能先造一份 `Plan`
fn grow_end_for_relocations(
    entries: &[RelocEntry],
    last_usable: u64,
    target_start: u64,
) -> io::Result<u64> {
    grow_end_of(entries.iter().map(|e| (e.src_first, e.delta)), last_usable, target_start)
}

/// `(旧起点, 位移)` 序列 → 扩容终点：min(新起点) − 1；无成员时取 `last_usable`。
/// 上、下两个边界在 [`grow_end_of`] 收口，plan 与记录两条入口共用
fn grow_end_of(
    targets: impl Iterator<Item = (u64, i64)>,
    last_usable: u64,
    target_start: u64,
) -> io::Result<u64> {
    let mut leftmost: Option<u64> = None;
    for (first_lba, delta_lba) in targets {
        // 端点只供扩容推导，左移的条目推不出扩容终点：不取绝对值顶替，直接拒绝
        if delta_lba < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "relocation moves left, so no grow endpoint can be derived from it — refusing",
            ));
        }
        let new_first = first_lba.checked_add_signed(delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        leftmost = Some(leftmost.map_or(new_first, |f| f.min(new_first)));
    }
    let grow_end = match leftmost {
        // moves 可整份来自 ckpt，减法同样 checked：不依赖"正常输入不会产生这种值"
        Some(f) => f.checked_sub(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target below LBA 1 — refusing")
        })?,
        None => last_usable,
    };
    if grow_end > last_usable {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "relocation target beyond last usable LBA — refusing"));
    }
    if grow_end < target_start {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "grow target below its own start — refusing"));
    }
    Ok(grow_end)
}

/// 续跑一致性：记录里的有序计划、双游标与几何守卫对上"盘上现在是什么"。
/// 全部检查在写任何字节之前完成，任一不成立都拒绝。`move` 与
/// `resize --allow-move` 共用这一份，不再各写一套。
///
/// 只读记录与几何，不按当前盘面重算 plan：盘上几何已被部分改写时，现算的 delta
/// 与记录不一致，续跑必须以记录为准。`grow_part == 0` 表示本记录不属于扩容作业
/// （`move`），扩容相关的两条守卫随之不适用。
///
/// 本函数是 `move` 与 `resize --allow-move` 的共享层，不依赖 `cmd::*`。
pub(crate) fn verify_relocation_resume(
    r: &Relocation,
    g: &ValidatedGeometry,
    disk_size: u64,
    chunk_len: u64,
) -> Result<(), RunOutcome> {
    // --chunk 是命令行可控项，单独先查：混进"盘上事实不符"里报会让用户去查盘，
    // 而这一项的直接原因是本次换了 --chunk
    if r.chunk_bytes != chunk_len {
        return Err(RunOutcome::refused(format!(
            "the recovery record was written with --chunk {} MiB, this run uses {} — re-run with the original value",
            r.chunk_bytes / (1024 * 1024),
            chunk_len / (1024 * 1024),
        )));
    }
    // 记录与盘同源的两维：换了目标尺寸或表扇区大小，游标到字节的映射不再成立
    if r.disk_size != disk_size || r.ss != g.sector_size() {
        return Err(RunOutcome::refused(
            "the recovery record does not match the current disk geometry — refusing",
        ));
    }
    // 与下面块数上界同一口径：entry_index 超出条目数是记录被改写或两处进度字段
    // 口径分叉的信号，夹紧只会把异常放行成「全部完成」
    if r.entry_index as usize > r.entries.len() {
        return Err(RunOutcome::refused(format!(
            "recovery record entry progress {} exceeds the plan's {} entries — the record changed between reads; refusing",
            r.entry_index,
            r.entries.len()
        )));
    }
    let done = r.entry_index;
    for (mi, e) in r.entries.iter().enumerate() {
        // 已完成的停在 first + delta（表项绝对赋值，重放幂等），未开始的仍在 first；
        // 正在搬的那一条可能已提交表项而 ckpt 尚未推进，故两个位置都接受
        let cur = g.entry_index(e.partition).and_then(|i| g.entries().get(i)).ok_or_else(|| {
            RunOutcome::refused(format!(
                "the recovery record names partition {} that is no longer on the disk — refusing",
                e.partition
            ))
        })?;
        let target = e
            .src_first
            .checked_add_signed(e.delta)
            .ok_or_else(|| RunOutcome::no_mutation("relocation target LBA overflows"))?;
        // 端点也要比：只比起点时，成员末端在中断窗口被改小/改大仍算"停在应停处"，
        // 而记录里的 len 会继续被当成搬移长度与扩容边界
        let src_end = e
            .src_first
            .checked_add(e.len)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| RunOutcome::no_mutation("relocation source range overflows"))?;
        let dst_end = target
            .checked_add(e.len)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| RunOutcome::no_mutation("relocation target range overflows"))?;
        let at_src = cur.starting_lba == e.src_first && cur.ending_lba == src_end;
        let at_dst = cur.starting_lba == target && cur.ending_lba == dst_end;
        let ok = match (mi as u32).cmp(&done) {
            std::cmp::Ordering::Less => at_dst,
            std::cmp::Ordering::Equal => at_src || at_dst,
            std::cmp::Ordering::Greater => at_src,
        };
        if !ok {
            return Err(RunOutcome::refused(format!(
                "partition {} is not where the recovery record says it should be (on disk LBA {}, expected {}) — the layout moved under the job; refusing",
                e.partition, cur.starting_lba, e.src_first
            )));
        }
    }
    // 目的区校验：上面的位置比对只覆盖源位置这半个维度。目的区同样是盘上可控值——
    // 中断期间第三方改表（尾打包的目的区恰在 last_usable 端空闲带，最易被新建分区
    // 占用），或记录被改写出 CRC 自洽的目的区。与**非本作业**条目重叠的续跑会把
    // chunk 拷贝写到别人的分区并提交重叠表项；本作业成员全部豁免——移位/尾打包的
    // 目的区合法落在其他成员的旧位
    let mut dst_ranges = Vec::with_capacity(r.entries.len());
    for e in &r.entries {
        let dst = e
            .src_first
            .checked_add_signed(e.delta)
            .ok_or_else(|| RunOutcome::no_mutation("relocation target LBA overflows"))?;
        let dst_end = dst
            .checked_add(e.len)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| RunOutcome::no_mutation("relocation target range overflows"))?;
        dst_ranges.push((e.partition, dst, dst_end));
    }
    for &(pn, dst, dst_end) in &dst_ranges {
        for (ei, e) in g.entries().iter().enumerate() {
            // 空条目（0,0）不可能命中：dst 下界 ≥ first_usable ≥ 2
            let n = ei as u32 + 1;
            if n == pn || r.entries.iter().any(|m| m.partition == n) {
                continue;
            }
            if e.starting_lba <= dst_end && dst <= e.ending_lba {
                return Err(RunOutcome::refused(format!(
                    "relocation target of partition {} (LBA {}..{}) overlaps partition {} on disk — the layout moved under the job; refusing",
                    pn, dst, dst_end, n
                )));
            }
        }
    }
    // 目的区两两不相交：重叠的目的区无论执行顺序如何都互相覆写
    for (i, &(apn, ad, ae)) in dst_ranges.iter().enumerate() {
        for &(bpn, bd, be) in dst_ranges.iter().skip(i + 1) {
            if ad <= be && bd <= ae {
                return Err(RunOutcome::refused(format!(
                    "relocation targets of partitions {} and {} overlap (LBA {}..{}) — inconsistent recovery record; refusing",
                    apn, bpn, ad, ae
                )));
            }
        }
    }
    // 顺序不变量：成员按降序处理（最右侧先搬），故一个目的区可以复写**已处理**
    // 成员的旧位，但不得覆盖尚未处理成员的源区——那次拷贝会读到已被本目的区覆写
    // 的数据
    for (i, &(pn, dst, dst_end)) in dst_ranges.iter().enumerate() {
        for m in r.entries.iter().skip(i + 1) {
            let src_end = m
                .src_first
                .checked_add(m.len)
                .and_then(|v| v.checked_sub(1))
                .ok_or_else(|| RunOutcome::no_mutation("relocation source range overflows"))?;
            if dst <= src_end && m.src_first <= dst_end {
                return Err(RunOutcome::refused(format!(
                    "relocation target of partition {} (LBA {}..{}) would overwrite the source of partition {} (LBA {}..{}) before it is copied — inconsistent recovery record; refusing",
                    pn, dst, dst_end, m.partition, m.src_first, src_end
                )));
            }
        }
    }
    if r.grow_part != 0 {
        // 扩容区间同样会写进表项并扩进 FS：中断期间第三方可能在"目标当前末端+1 ..
        // 最左目的区−1"的空闲段新建分区（它与任何目的区都不重叠），照旧扩到
        // grow_end 就会写出重叠表并把 FS 扩进该分区。成员豁免：grow_end 恒小于
        // 每个成员的新起点，而新起点不小于其源起点
        let ge = gpt_policy::live_entry(g, r.grow_part)?;
        let future_grow_end = grow_end_for_relocations(&r.entries, g.last_usable_lba(), ge.starting_lba)?;
        // 缩方向同样要拒：中断期间第三方可以把目标扩到 grow_end 之外，照 ckpt
        // 收尾就会把表项缩回去，而 FS 已按更大的分区扩过
        if future_grow_end < ge.ending_lba {
            return Err(RunOutcome::refused(format!(
                "partition {} already ends at LBA {}, past the recovery record's grow target {} — \
                 finishing would shrink it below a filesystem that may already fill it; refusing",
                r.grow_part, ge.ending_lba, future_grow_end
            )));
        }
        if future_grow_end > ge.ending_lba {
            for (ei, e) in g.entries().iter().enumerate() {
                let n = ei as u32 + 1;
                if n == r.grow_part || r.entries.iter().any(|m| m.partition == n) {
                    continue;
                }
                if e.starting_lba <= future_grow_end && e.ending_lba > ge.ending_lba {
                    return Err(RunOutcome::refused(format!(
                        "the grown partition would extend into partition {} (LBA {}..{}) created since the record was written — the layout moved under the job; refusing",
                        n, e.starting_lba, e.ending_lba
                    )));
                }
            }
        }
    }
    // 块数上界：越界不是合法运行能产出的进度，静默 min 会把"跳过未拷贝的 chunk"
    // 放行成提交。布局规则只来自 [`entry_spans`]
    let chunk_bound = r.entries.get(done as usize).map_or(0, |e| {
        entry_spans(e.src_first, e.len, e.delta, e.is_swap, r.ss, r.chunk_bytes).len() as u64
    });
    if r.chunks_in_entry > chunk_bound {
        return Err(RunOutcome::refused(format!(
            "recovery record chunk progress {} exceeds the bound {} fixed in the plan — the record changed between reads; refusing",
            r.chunks_in_entry, chunk_bound
        )));
    }
    Ok(())
}