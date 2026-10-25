//! 搬移计划的推导：由几何、表项与续传状态算出每步动作和端点。纯判定，不写盘。

use super::*;

/// chunk 字节数：命令行 --chunk-size（MiB）传入；checkpoint 记录该值，
/// 续传时不一致即拒绝（chunks_done 是按 chunk 计数的，换大小会错位）
pub fn chunk_bytes(mib: u64) -> io::Result<u64> {
    if !(1..=1024).contains(&mib) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--chunk-size out of range (1..=1024 MiB)"));
    }
    Ok(mib * 1024 * 1024)
}

/// 拒绝搬移（起始 LBA 变化）的 GPT 类型 GUID：LUKS/LVM PV——搬移后其元数据语义不明。
/// 纯扩缩（起点不动、数据不搬）不受此限。swap 不在本判据：它在 plan 层有专门处理
/// （内容可弃 → 不搬数据、落位后 mkswap 重建），只有 resize-part 的单分区路径会
/// 一并拒绝它（那里没有重建流程）
///
/// 磁盘序的常量取自 [`table`]；下面两个是同一标准文本的自然序形态（按 RFC 4122
/// 字段顺序摆放，而非 GPT 要求的混合端）。两种布局的盘都真实存在，识别两种都认
pub(crate) const LUKS_TYPE_GUID_NATURAL: [u8; 16] = [
    0xCA, 0x7D, 0x7C, 0xCB, 0x63, 0xED, 0x4C, 0x53, 0x86, 0x1C, 0x17, 0x42, 0x53, 0x60, 0x59, 0xCC,
];

pub(crate) const LVM_TYPE_GUID_NATURAL: [u8; 16] = [
    0xE6, 0xD6, 0xD3, 0x79, 0xF5, 0x07, 0x44, 0xC2, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28,
];

/// LUKS / LVM PV 类型判定（磁盘序与 table 常量同源，不另抄字面量）
pub(crate) fn is_forbidden_type_guid(g: &[u8; 16]) -> bool {
    *g == table::LUKS_TYPE_GUID
        || *g == LUKS_TYPE_GUID_NATURAL
        || *g == table::LVM_TYPE_GUID
        || *g == LVM_TYPE_GUID_NATURAL
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

/// swap 类型 GUID 的自然序形态：同一标准文本按 RFC 4122 字段顺序摆放（而非 GPT 要求的
/// 混合端）。识别两种都认——两种布局的盘都真实存在
pub(crate) const SWAP_TYPE_GUID_NATURAL: [u8; 16] = [
    0x06, 0x57, 0xFD, 0x6D, 0xA4, 0xAB, 0x43, 0xC4, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
];

/// swap 类型判定（磁盘序与 table::SWAP_TYPE_GUID 同源，不另抄字面量）
pub(crate) fn is_swap_guid(g: &[u8; 16]) -> bool {
    *g == table::SWAP_TYPE_GUID || *g == SWAP_TYPE_GUID_NATURAL
}

/// plan 的进入语义。`grow` 与 `SIZE` 两条命令共用同一个续跑槽位，续跑时必须
/// 对上号：按自己的语义执行对方的 plan，就是把用户的请求静默放大或缩水
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PlanKind {
    /// 吃满右侧空闲的尾部打包（`resize grow` / `plan` / `apply`）
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
    /// 待执行的修复动作：plan 只记录，apply 执行（plan 本身不写盘）
    pub repair: RepairAction,
}

/// movable 集合与两条硬拒绝的唯一构造处：目标之后所有已定义条目（按起点升序）。
/// 端点倒挂（盘内容故障）与 LUKS/LVM PV（不可搬移）在此一并给出——
/// 两条规划路径共用，拒绝理由不会漂移
pub(crate) fn blockers_after(
    g: &ValidatedGeometry,
    target_end: u64,
) -> Result<Vec<(u32, &gptman::GPTPartitionEntry)>, Fail> {
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
            return Err(Fail::infra(format!("partition {num} has ending LBA below starting LBA (corrupted table)")));
        }
        if is_forbidden_type_guid(&e.partition_type_guid) {
            return Err(Fail::refused(format!("partition {num} is LUKS/LVM PV — relocation refused")));
        }
    }
    Ok(movable)
}

/// plan 生成：只解析 + 验证 + 计算，不写盘。stale 表按"修复后"的几何生成计划，
/// 并把修复动作记入 plan.repair，由 apply 执行。
/// 尾打包规划的唯一实现处：几何已由调用方解析，本函数不再读盘、不再重新推导边界。
/// 生产路径只有恢复感知入口（[`make_plan_resuming`]，几何解析一次），本函数由它直接调用
pub(crate) fn plan_tail_packed(g: &ValidatedGeometry, repair: RepairAction, grow_part: u32) -> Result<Plan, Fail> {
    let ss = g.sector_size();

    // 目标分区必须存在（唯一查找出口见 live_entry）
    let target = gpt_policy::live_entry(g, grow_part)?;

    let movable = blockers_after(g, target.ending_lba)?;

    let mut moves = Vec::new();
    let mut cursor = g.last_usable_lba();
    for (num, e) in movable.iter().rev() {
        let len = e.ending_lba - e.starting_lba + 1;
        // 尾部打包：末端对齐 cursor（含），不留缝隙
        let new_first = cursor.checked_sub(len - 1).ok_or_else(|| {
            Fail::refused("insufficient tail space for relocation")
        })?;
        // 目标起点低于源起点 = 尾部空间不足
        let delta = new_first.checked_sub(e.starting_lba).ok_or_else(|| {
            Fail::refused("insufficient tail space for relocation")
        })?;
        // saturating：new_first 理论上 ≥1（LBA0 保护 MBR + LBA1 主头），
        // 极端输入下防下溢 panic，退化为 0 后由下轮 checked_sub 报空间不足
        cursor = new_first.saturating_sub(1);
        moves.push(PlanEntry { part_num: *num, first_lba: e.starting_lba, len_lba: len, delta_lba: delta, is_swap: is_swap_guid(&e.partition_type_guid) });
    }
    Ok(Plan { ss, last_usable_lba: g.last_usable_lba(), grow_part, moves, kind: PlanKind::TailPacked, repair })
}

/// 尾打包 plan 的恢复感知版本：ckpt 存在时以 ckpt 的 moves 为准（见 resume_outcome），
/// 否则照常现算。几何与修复动作由调用方解析（一条命令解析一次，往下传），本函数不读表
pub fn make_plan_resuming(
    src: &FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<Plan, Fail> {
    match resume_outcome(src, g, grow_part)? {
        // 作业身份比对：本入口的进入语义是"吃满右侧"（TailPacked）。槽上那份若是精确
        // SIZE 的最小位移 plan，它的终点停在旧请求的末端——按 grow 收尾等于把本次请求
        // 静默换成旧作业的续跑。判据与 SIZE 入口共用一处（见 make_plan_shift_resuming）
        ResumeOutcome::Belongs(p) => match p.kind {
            PlanKind::TailPacked => Ok(p),
            PlanKind::Shift => Err(plan_job_owns_target(p.kind, false)),
        },
        // 槽位是别的分区的作业：现算的 plan 与那份作业的现场不符，打印出来会与
        // 盘上未收尾的事自相矛盾，apply 也只会得到一次注定失败的比对
        ResumeOutcome::Occupied => Err(foreign_checkpoint_refusal()),
        // 槽位被单分区 resize 作业占着：与 apply 同一结论、同一文案（那里在 prepare_apply
        // 的分派处拒绝）。只修 apply 不修这里，plan 就会打印一份 apply 必然拒绝的计划并报成功
        ResumeOutcome::ForeignResize => Err(foreign_resize_refusal()),
        ResumeOutcome::Empty => plan_tail_packed(g, repair, grow_part),
    }
}

/// 最小位移 plan：目标分区增长 `shift` 扇区，其右侧所有挡路分区右移让位。
/// 与 make_plan（尾打包）相对，用于精确大小的扩容（resize SIZE + --allow-move）。
/// 落点是绝对量（同 parted `move START END` 的语义）：首个挡路分区新起点恰为
/// 目标原末端 + shift + 1，apply 的扩容终点（min(new_first) − 1）因此精确落在
/// 请求的新末端——既不多吞目标与挡路者之间的间隙，也不少给。
/// 后续挡路者取"紧随前一落点"与"当前位置"的较大者：已在目标位的挡路者得到
/// delta 0、apply 跳过。计划生成因此对部分位移后的盘面幂等——中断后 abandon 重跑
/// （此时 ckpt 已删，必然现算）生成与一次做成相同的落点，不会对已搬段二次位移
///
/// moves 按起始 LBA 降序排列（apply 升序处理 → 最右侧先搬）：
/// 各分区的目的区要么落在尾部空闲，要么落在其右侧已被搬空分区的旧位置，拷贝永不踩源
/// 最小位移规划的唯一实现处：几何已由调用方解析，本函数不再读盘
pub(crate) fn shift_plan_from(g: &ValidatedGeometry, repair: RepairAction, grow_part: u32, shift: u64) -> Result<Plan, Fail> {
    let ss = g.sector_size();

    let target = gpt_policy::live_entry(g, grow_part)?;
    if shift == 0 {
        return Err(Fail::refused("shift must be positive"));
    }

    let movable = blockers_after(g, target.ending_lba)?;
    if movable.is_empty() {
        return Err(Fail::refused("no partition to relocate (right side is free — no --allow-move needed)"));
    }
    if shift < movable[0].1.starting_lba - target.ending_lba {
        return Err(Fail::refused("shift does not exceed the free gap before the first blocker — relocation not needed"));
    }

    let mut moves: Vec<PlanEntry> = Vec::with_capacity(movable.len());
    let mut cursor = target.ending_lba.checked_add(shift)
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| Fail::refused("relocation target LBA overflows — refusing"))?;
    for (num, e) in &movable {
        let len = e.ending_lba - e.starting_lba + 1;
        let new_first = cursor.max(e.starting_lba);
        let delta = new_first - e.starting_lba;
        let new_end = new_first.checked_add(len - 1).ok_or_else(|| {
            Fail::refused("relocation target LBA overflows — refusing")
        })?;
        if new_end > g.last_usable_lba() {
            return Err(Fail::refused("insufficient tail space to relocate blockers by the requested size"));
        }
        moves.push(PlanEntry { part_num: *num, first_lba: e.starting_lba, len_lba: len, delta_lba: delta, is_swap: is_swap_guid(&e.partition_type_guid) });
        cursor = new_end.saturating_add(1);
    }
    moves.reverse(); // 升序构造 → 降序排列：最右侧先搬
    Ok(Plan { ss, last_usable_lba: g.last_usable_lba(), grow_part, moves, kind: PlanKind::Shift, repair })
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

/// 中断感知的最小位移 plan：ckpt 存在时以 ckpt 的 moves 为准（见 resume_outcome），
/// 否则按 `intent` 现算。`Shrink` 一律拒绝并指向续跑/放弃两条出路——槽上未收尾的
/// plan 属于当初那条扩容请求，执行它就是把缩容静默放大成旧扩容（违反 plan 的
/// "执行方即请求方"归属）。`NoOp` 只在表已提交、只剩收尾时出现：只有 kind 相符且
/// 计划终点恰等于当前末端，才是同一作业的续跑。几何与修复动作由调用方解析并下传，
/// 本函数不读表
pub fn make_plan_shift_resuming(
    src: &FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    intent: ShiftIntent,
) -> Result<Plan, Fail> {
    match resume_outcome(src, g, grow_part)? {
        ResumeOutcome::Belongs(p) => {
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
                        .ok_or_else(|| Fail::refused("requested size overflows the LBA range"))?;
                    if plan_requested_end(&p, e.starting_lba) != Some(want_end) {
                        return Err(plan_job_owns_target(p.kind, true));
                    }
                    return Ok(p);
                }
            }
        }
        // 与 make_plan_resuming 同判据：别的分区的作业占着槽位，本请求既不能把它
        // 当空槽、也不能按它续跑
        ResumeOutcome::Occupied => return Err(foreign_checkpoint_refusal()),
        // 另一族作业（单分区 resize）占着槽位：同上——现算的位移 plan 与那个作业的现场
        // 不符，它的续跑点由 prepare_resize 按 RsCheckpoint 决定，不在这里
        ResumeOutcome::ForeignResize => return Err(foreign_resize_refusal()),
        ResumeOutcome::Empty => {}
    }
    let ShiftIntent::Grow(shift) = intent else {
        return Err(Fail::refused(
            "shrink does not relocate partitions — this entry point only builds relocation plans for growth",
        ));
    };
    shift_plan_from(g, repair, grow_part, shift)
}

/// 该分区是否有未收尾的 plan 型搬移作业。命令入口据此分流：几何上"右侧已空、可直接扩容"
/// 并不代表作业已完成——右侧变空本身可能正是搬了一半的结果，收尾步骤（剩余搬移、swap 重建、
/// FS 扩容）都还没做。几何由调用方解析并下传
pub fn has_pending_relocation(src: &FileSource, g: &ValidatedGeometry, grow_part: u32) -> Result<bool, Fail> {
    Ok(matches!(resume_outcome(src, g, grow_part)?, ResumeOutcome::Belongs(_)))
}

/// 事务门用的归属判定：与 `has_pending_relocation` 同一内核，但把"槽位属于别的分区"
/// 如实报成 `Err`。压成 `false` 的后果是用户只看到通用的"目标被占着"，而真正的原因
/// （该去重跑哪个命令）丢失在领域层
pub(crate) fn relocation_ownership(src: &FileSource, grow_part: u32) -> Result<bool, Fail> {
    match resume_outcome_in(src, grow_part)? {
        ResumeOutcome::Belongs(_) => Ok(true),
        // 空槽与"另一族作业占着"对本事务门是同一结论：都不由它续跑（单分区
        // resize 的现场要用 resize-part / move 续，不是重跑 resize），闸口按 busy 拒绝
        ResumeOutcome::Empty | ResumeOutcome::ForeignResize => Ok(false),
        ResumeOutcome::Occupied => Err(foreign_checkpoint_refusal()),
    }
}

/// 槽位被**别的分区**的搬移作业占用时的拒绝理由。两处 plan 生成路径与事务门共用
/// 同一句话：同一事实的措辞只写一次，否则用户在不同入口看到的原因迟早会漂移
pub(crate) fn foreign_checkpoint_refusal() -> Fail {
    Fail::refused(
        "an unfinished relocation job for a different partition occupies the checkpoint slot — re-run that job's command to finish it first",
    )
}

/// 槽位被**另一族**作业（单分区 resize）占用时的拒绝理由。`prepare_apply` 的分派处与
/// `make_plan_resuming` 共用同一句话：只修一处会让 plan 打印一份 apply 必然拒绝的计划，
/// 而两边给同一事实的措辞也会漂移
pub(crate) fn foreign_resize_refusal() -> Fail {
    Fail::refused(
        "an unfinished single-partition resize job occupies the checkpoint slot — resume it with `resize-part` or `move`, or release it with `diskedit abandon`",
    )
}

/// 续跑入口的作业身份不匹配时的拒绝理由。措辞按 pending 作业的进入语义分句：
/// 用户拿到的必须是**唯一**能接着做下去的那条命令。四个续跑入口（grow / SIZE /
/// plan / apply）共用这一处——只修其中一处，同一条事实在不同入口的指引迟早漂移
pub(crate) fn plan_job_owns_target(kind: PlanKind, different_size: bool) -> Fail {
    let verb = match kind {
        PlanKind::TailPacked => "`resize grow`",
        PlanKind::Shift => "`resize SIZE --allow-move`",
    };
    if different_size {
        Fail::refused(format!(
            "an unfinished {verb} job owns this target for a different size — re-run that command with its original size, or `diskedit abandon` to release it",
        ))
    } else {
        Fail::refused(format!(
            "an unfinished {verb} job owns this target — re-run that command to resume it, or `diskedit abandon` to release it",
        ))
    }
}

/// 作业身份里的"请求数值"一维：Shift plan 的扩容终点（`min(new_first) − 1`，与 apply
/// 实际写下的终点是同一实现）。TailPacked 的终点由几何决定、没有数值维度，返回 None
/// 表示"这一维不参与比对"；几何非法时同样返回 None——比对不上即拒绝，不把损坏现场
/// 当成"身份相符"放行
pub(crate) fn plan_requested_end(plan: &Plan, target_start: u64) -> Option<u64> {
    match plan.kind {
        PlanKind::TailPacked => None,
        PlanKind::Shift => grow_end_for(plan, target_start).ok(),
    }
}

/// 三态判定的共用内核：几何在内部解析（调用方手上没有已验证几何）
pub(crate) fn resume_outcome_in(src: &FileSource, grow_part: u32) -> Result<ResumeOutcome, Fail> {
    let (g, _repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    resume_outcome(src, &g, grow_part)
}

/// resume 判定的三态结论——刻意不压扁成 Option："没有作业"、"是本次请求的作业"、
/// "槽位被别的作业占用"在调用方处各走各路：只有第一种允许现算新计划；第二种以
/// ckpt 为准续跑；第三种必须拒绝——当空槽会覆盖别人的现场，当自己的续跑点会执行
/// 错误的 plan（盘上几何已被那次作业部分改写，现算出的 delta 与其记录不一致）
pub(crate) enum ResumeOutcome {
    Empty,
    /// ckpt 属于本次请求的分区：plan 自持（盘上几何已被部分执行改变，现算必错）
    Belongs(Plan),
    /// 槽位被别的分区的 relocation ckpt 占用；`has_pending_relocation` 只回答"plan 型
    /// 搬移"，搬移路径必须拒绝——现算的 plan 与那份作业的现场不符
    Occupied,
    /// 槽位被**另一族**作业（单分区 resize 的 `RsCheckpoint`）占用。与 `Occupied` 并列而非
    /// 并入，因为两族对它的结论不同：plan 型搬移绝不能在此现算计划——盘上几何已被那次
    /// resize 部分改写，现算的 plan 会打印成功，apply 却必然拒绝；resize/apply 对它
    /// 拒绝（本入口续不了它，出路是 `resize-part` / `move`）
    ForeignResize,
}

/// ckpt 里记录的 plan（仅当它属于该分区）。plan 型搬移的**任何**生成路径在恢复期都必须
/// 以它为准：盘上几何已被部分执行改变，重算出的 delta 与 ckpt 记的不一致，会撞上 apply
/// 的恢复校验而使续传永久失败。ckpt 存在 ⇒ repair 已在 apply 开头执行过（在初始 ckpt
/// 写入之前），故 repair 记 None
pub(crate) fn resume_outcome(src: &FileSource, g: &ValidatedGeometry, grow_part: u32) -> Result<ResumeOutcome, Fail> {
    let (slot, _) = read_checkpoint(src, g)?;
    Ok(match slot {
        CheckpointSlot::Ambiguous(paths) => return Err(ambiguous_checkpoint(&paths)),
        CheckpointSlot::Relocation(c) => {
            let c = *c;
            if c.grow_part == grow_part {
                ResumeOutcome::Belongs(Plan {
                    ss: c.ss,
                    last_usable_lba: c.last_usable_lba,
                    grow_part: c.grow_part,
                    moves: c.moves,
                    kind: c.kind,
                    repair: RepairAction::None,
                })
            } else {
                ResumeOutcome::Occupied
            }
        }
        CheckpointSlot::Empty => ResumeOutcome::Empty,
        // 另一族作业（单分区 resize）占着槽位：搬移不得在此现算计划，见 `ForeignResize`
        CheckpointSlot::Resize(_) => ResumeOutcome::ForeignResize,
    })
}

// ---------- checkpoint ----------
//
// 落盘状态的判据：**只持久化盘上无法判定到可枚举有限状态的事实**。按它三选一——
// "表项是否已提交"由盘上的分区几何直接给出（commit 写绝对 LBA、重放幂等），不另存；
// "FS 是否已缩"算得出当前大小、算不出历史（缩过就再也看不出来），必须存（fs_shrunk）；
// "搬到第几 chunk"从数据区只看得出"已覆盖"、看不出"到哪"，必须存（chunks_done）。
// 本文两个 checkpoint（搬移的 Checkpoint 与单分区缩放的 RsCheckpoint）同受此判据约束

/// 一条搬移的块布局：返回 `(每块字节数, 块数上界)`。
///
/// 逐块前移的覆盖顺序要求**自重叠**搬移（位移 < 源长度）的单次拷贝不超过位移，
/// 否则本次拷贝会写进自己还没读的源区。故有效块大小取 `chunk_bytes` 与位移字节的
/// 较小者。**唯一实现**：搬移循环、checkpoint 采纳与续跑校验都从这里取——同一算式
/// 抄在多处时，任何一处漂移都会变成"少拷却提交表项"或"重做读到已被覆盖的源"。
///
/// `copy_lba` 是本次要搬的扇区数（plan 条目 = 分区长度；move+shrink = 新旧区间的
/// 较小者），`src_len_lba` 是源区间长度：`shift != 0 && shift < src_len_lba` 即自重叠。
/// 位移为 0 的条目与块大小算成 0 的现场都没有可用的块布局，返回 `None`
/// （调用方按"溢出/损坏"拒绝，不拿 0 当除数）
pub(crate) fn move_chunk_layout(
    copy_lba: u64,
    ss: u64,
    shift_lba: u64,
    src_len_lba: u64,
    chunk_bytes: u64,
) -> Option<(u64, u64)> {
    let chunk = if shift_lba != 0 && shift_lba < src_len_lba {
        chunk_bytes.min(shift_lba.checked_mul(ss)?)
    } else {
        chunk_bytes
    };
    if chunk == 0 {
        return None;
    }
    let count = copy_lba.checked_mul(ss)?.checked_add(chunk - 1)? / chunk;
    Some((chunk, count))
}

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