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

/// 拒绝搬移（起始 LBA 变化）的 GPT 类型 GUID：LUKS/LVM PV——搬移后其元数据语义不明。
/// 纯扩缩（起点不动、数据不搬）不受此限。swap 不在本判据：它在 plan 层有专门处理
/// （内容可弃 → 不搬数据、落位后 mkswap 重建），只有 resize-part 的单分区路径会
/// 一并拒绝它（那里没有重建流程）
///
/// 类型 GUID 一律按 UEFI 规范磁盘序比较（前三字段小端、Data4 原序），不做反向编码容错
pub(crate) fn is_forbidden_type_guid(g: &[u8; 16]) -> bool {
    [table::LUKS_TYPE_GUID, table::LVM_TYPE_GUID].contains(g)
}

#[derive(Clone)]
pub struct PlanEntry {
    pub part_num: u32,
    pub first_lba: u64,
    pub len_lba: u64,
    pub delta_lba: u64,
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
/// 端点倒挂与不可搬移类型已由 [`blockers_after`] 拒绝，这里只处理落点算术
fn relocate_entry(
    num: u32,
    e: &gptman::GPTPartitionEntry,
    new_first: u64,
    last_usable: u64,
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
    Ok(PlanEntry {
        part_num: num,
        first_lba: e.starting_lba,
        len_lba: len,
        delta_lba: delta,
        is_swap: is_swap_guid(&e.partition_type_guid),
    })
}

/// 几何求解器：plan 生成只解析 + 验证 + 计算，不写盘。stale 表按「修复后」的几何
/// 求解，修复动作记入 `plan.repair`，由执行链落实。几何由调用方解析一次并下传，本函数
/// 不再读盘、不再重新推导边界。
///
/// 两种意图共用同一份目标查找、阻挡集合、单条目落位与计划装配；策略只决定每条目的
/// 新起点。输出统一按起始 LBA 降序（最右侧先搬），使拷贝永不踩源
fn solve_layout(
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    layout: Layout,
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
                out.push(relocate_entry(*num, e, new_first, last_usable)?);
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
                out.push(relocate_entry(*num, e, new_first, last_usable)?);
                let len = e.ending_lba - e.starting_lba + 1;
                cursor = new_first
                    .checked_add(len)
                    .ok_or_else(|| RunOutcome::refused("relocation target LBA overflows — refusing"))?;
            }
            out.reverse(); // 升序构造 → 降序排列
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
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<Plan, RunOutcome> {
    solve_layout(g, repair, grow_part, Layout::TailPacked)
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
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    shift: u64,
) -> Result<Plan, RunOutcome> {
    solve_layout(g, repair, grow_part, Layout::Shift(shift))
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
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<Plan, RunOutcome> {
    match resume_outcome(relocation, g, grow_part)? {
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
        ResumeRunOutcome::Empty => plan_tail_packed(g, repair, grow_part),
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
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    intent: ShiftIntent,
) -> Result<Plan, RunOutcome> {
    match resume_outcome(relocation, g, grow_part)? {
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
    shift_plan_from(g, repair, grow_part, shift)
}

/// 该分区是否有未收尾的 plan 型搬移作业。几何上"右侧已空、可直接扩容"并不代表
/// 作业已完成——右侧变空本身可能正是搬了一半的结果，收尾步骤（剩余搬移、swap 重建、
/// FS 扩容）都还没做。当前调用方是 movepart 的测试
#[cfg(test)]
pub fn has_pending_relocation(
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
    grow_part: u32,
) -> Result<bool, RunOutcome> {
    Ok(matches!(resume_outcome(relocation, g, grow_part)?, ResumeRunOutcome::Belongs(_)))
}

/// 槽位被**别的分区**的搬移作业占用时的拒绝理由。三处 plan 生成路径共用同一句话：
/// 同一事实的措辞只写一次，否则用户在不同入口看到的原因迟早会漂移。这里不知道那份
/// 作业的形状，所以只说"它属于创建它的那条请求"，不把重跑说成通用的出路
pub(crate) fn foreign_checkpoint_refusal() -> RunOutcome {
    RunOutcome::refused(
        "an unfinished relocation job for a different partition is recorded on this target — it belongs \
         to the request that created it: finish it with that request, or release it with `diskedit abandon`",
    )
}

/// 续跑入口的作业身份不匹配时的拒绝理由。措辞按 pending 作业的进入语义分句：
/// 用户拿到的必须是**唯一**能接着做下去的那条命令。两个续跑入口（`resize grow` /
/// `resize SIZE + --allow-move`）与只读预览 `plan --grow` 共用这一处——只修其中一处，
/// 同一条事实在不同入口的指引迟早漂移。
/// 两种形状的重跑都是续跑：最小位移作业按原尺寸接着做，尾部打包作业按记录里的
/// 有序计划接着做（提交前区间与计划同一次落盘，见 `shift::execute_apply`）
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
/// 记录不一致，会撞上执行器的恢复校验而使续传永久失败。记录存在 ⇒ repair 已在执行链
/// 开头执行过（在首次落盘之前），故 repair 记 None
pub(crate) fn resume_outcome(
    relocation: Option<&Relocation>,
    g: &ValidatedGeometry,
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
        repair: RepairAction::None,
    }))
}

/// resume 判定的三态结论——刻意不压扁成 Option："没有作业"、"是本次请求的作业"、
/// "槽位被别的作业占用"在调用方处各走各路：只有第一种允许现算新计划；第二种以
/// ckpt 为准续跑；第三种必须拒绝——当空槽会覆盖别人的现场，当自己的续跑点会执行
/// 错误的 plan（盘上几何已被那次作业部分改写，现算出的 delta 与其记录不一致）
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
    let mut leftmost: Option<u64> = None;
    for m in &plan.moves {
        let new_first = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        leftmost = Some(leftmost.map_or(new_first, |f| f.min(new_first)));
    }
    let grow_end = match leftmost {
        // moves 可整份来自 ckpt，减法同样 checked：不依赖"正常输入不会产生这种值"
        Some(f) => f.checked_sub(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target below LBA 1 — refusing")
        })?,
        None => plan.last_usable_lba,
    };
    if grow_end > plan.last_usable_lba {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "relocation target beyond last usable LBA — refusing"));
    }
    if grow_end < target_start {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "grow target below its own start — refusing"));
    }
    Ok(grow_end)
}